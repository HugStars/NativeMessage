use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::Serializer;
use serde_json::ser::PrettyFormatter;
use std::env;
use std::fs;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::panic;
use std::path::{Path, PathBuf};

// ---------- ANSI 颜色常量 ----------
const GREEN: &str = "\x1b[32m";
const BLUE: &str = "\x1b[34m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

// ---------- 固定常量 ----------
const HOST_NAME: &str = "top.hugstars.native_message";
const FOLDER_NAME: &str = "NativeMessage";
const LEGACY_FOLDER_NAME: &str = ".native_message";
const PLACEHOLDER_ORIGIN: &str = "chrome-extension://这里填入你的Chrome插件的ID/";
const MAX_RESPONSE_SIZE: usize = 1024 * 1024; // 1MB
const MAX_REQUEST_SIZE: usize = 4 * 1024 * 1024; // 4MB，防止异常长度前缀导致巨额分配

// ---------- Windows API ----------
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileType(hFile: *mut core::ffi::c_void) -> u32;
    fn FreeConsole() -> i32;
}

// stdin 是否为控制台（Chrome 拉起时是管道）
#[cfg(windows)]
fn stdin_is_console() -> bool {
    use std::os::windows::io::AsRawHandle;
    const FILE_TYPE_CHAR: u32 = 0x0002; // FILE_TYPE_CHAR = 控制台
    let handle = std::io::stdin().as_raw_handle();
    unsafe { GetFileType(handle) == FILE_TYPE_CHAR }
}

// 通信模式下脱离控制台，销毁 Chrome 拉起时产生的黑窗口
#[cfg(windows)]
fn detach_console() {
    unsafe {
        FreeConsole();
    }
}

#[cfg(unix)]
fn stdin_is_console() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

#[cfg(unix)]
fn detach_console() {}

fn log_err(msg: &str) {
    let _ = writeln!(std::io::stderr(), "{}[错误]{} {}", RED, RESET, msg);
}

// ---------- 数据结构 ----------
#[derive(Debug, Deserialize)]
struct Request {
    file_path: String,
    content: Option<String>,
    #[serde(default = "default_encoding")]
    encoding: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    length: Option<u64>,
}

fn default_encoding() -> String {
    "base64".to_string()
}

#[derive(Debug, Serialize)]
struct Response {
    status: String, // "ok" 或 "error"
    content: Option<String>,
    message: Option<String>,
    size: Option<u64>,
}

// ---------- 数据目录（%LOCALAPPDATA%\NativeMessage） ----------
fn data_dir() -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .ok_or_else(|| anyhow!("无法获取本地应用数据目录（AppData\\Local）"))?;
    Ok(base.join(FOLDER_NAME))
}

// ---------- 路径解析 ----------
fn resolve_path(raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        let base = data_dir().unwrap_or_else(|_| PathBuf::from("."));
        base.join(raw)
    }
}

// ---------- 路径安全校验（仅允许数据目录及其子目录） ----------
fn is_path_allowed(requested_path: &str) -> bool {
    let base = match data_dir() {
        Ok(b) => b,
        Err(_) => return false,
    };
    let canonical_base = match base.canonicalize() {
        Ok(b) => b,
        Err(_) => return false,
    };
    // 从目标路径向上找到最近的存在祖先（写入时尚不存在的文件/目录也能通过校验）
    let mut cur = Some(Path::new(requested_path));
    while let Some(p) = cur {
        if let Ok(canonical) = p.canonicalize() {
            return canonical.starts_with(&canonical_base);
        }
        cur = p.parent();
    }
    false
}

// ---------- 处理读取 ----------
fn handle_read(
    file_path: &str,
    encoding: &str,
    offset: Option<u64>,
    length: Option<u64>,
) -> Response {
    // 1. 打开文件
    let file = match fs::File::open(file_path) {
        Ok(f) => f,
        Err(e) => {
            return Response {
                status: "error".to_string(),
                content: None,
                message: Some(format!("打开文件失败：{}", e)),
                size: None,
            };
        }
    };

    // 2. 获取文件大小
    let total_size = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            return Response {
                status: "error".to_string(),
                content: None,
                message: Some(format!("获取文件大小失败：{}", e)),
                size: None,
            };
        }
    };

    // 3. 确定读取范围
    let start = offset.unwrap_or(0);
    let mut read_len = length.unwrap_or(total_size);
    if start >= total_size {
        return Response {
            status: "error".to_string(),
            content: None,
            message: Some("偏移量超出文件大小".to_string()),
            size: Some(total_size),
        };
    }
    if start + read_len > total_size {
        read_len = total_size - start;
    }
    if read_len == 0 {
        return Response {
            status: "ok".to_string(),
            content: Some("".to_string()),
            message: None,
            size: Some(total_size),
        };
    }

    // 4. 定位并读取
    let mut file = file;
    if let Err(e) = file.seek(SeekFrom::Start(start)) {
        return Response {
            status: "error".to_string(),
            content: None,
            message: Some(format!("定位文件失败：{}", e)),
            size: Some(total_size),
        };
    }

    let mut buffer = vec![0u8; read_len as usize];
    if let Err(e) = file.read_exact(&mut buffer) {
        return Response {
            status: "error".to_string(),
            content: None,
            message: Some(format!("读取文件失败：{}", e)),
            size: Some(total_size),
        };
    }

    // 5. Base64 编码
    let content_encoded = if encoding == "text" {
        match String::from_utf8(buffer.clone()) {
            Ok(text) => text,
            Err(_) => BASE64.encode(&buffer),
        }
    } else {
        BASE64.encode(&buffer)
    };

    Response {
        status: "ok".to_string(),
        content: Some(content_encoded),
        message: None,
        size: Some(total_size),
    }
}

// ---------- 处理写入 ----------
fn handle_write(file_path: &str, content_b64: &str) -> Response {
    match BASE64.decode(content_b64) {
        Ok(data) => {
            if let Some(parent) = Path::new(file_path).parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    return Response {
                        status: "error".to_string(),
                        content: None,
                        message: Some(format!("创建父目录失败：{}", e)),
                        size: None,
                    };
                }
            }
            match fs::write(file_path, &data) {
                Ok(_) => Response {
                    status: "ok".to_string(),
                    content: None,
                    message: Some("文件写入成功".to_string()),
                    size: Some(data.len() as u64),
                },
                Err(e) => Response {
                    status: "error".to_string(),
                    content: None,
                    message: Some(format!("写入文件失败：{}", e)),
                    size: None,
                },
            }
        }
        Err(e) => Response {
            status: "error".to_string(),
            content: None,
            message: Some(format!("Base64 解码失败：{}", e)),
            size: None,
        },
    }
}

// ---------- 处理请求 ----------
fn process_request(request: Request) -> Response {
    // 解析路径
    let full_path = resolve_path(&request.file_path);
    let full_path_str = full_path.to_string_lossy().to_string();

    // 安全检查
    if !is_path_allowed(&full_path_str) {
        return Response {
            status: "error".to_string(),
            content: None,
            message: Some(format!(
                "拒绝访问：路径必须在 {} 数据目录下",
                FOLDER_NAME
            )),
            size: None,
        };
    }

    // 根据是否有 content 字段决定读写
    if let Some(content_b64) = request.content {
        handle_write(&full_path_str, &content_b64)
    } else {
        handle_read(
            &full_path_str,
            &request.encoding,
            request.offset,
            request.length,
        )
    }
}

// ---------- 发送响应（带大小检查） ----------
fn send_response(response: &Response) -> Result<()> {
    // 先尝试序列化
    let json = serde_json::to_string(response).map_err(|e| anyhow!("序列化响应失败：{}", e))?;
    let json_bytes = json.as_bytes();

    // 如果响应大小超过限制，构造一个简化的错误响应
    if json_bytes.len() > MAX_RESPONSE_SIZE {
        let error_resp = Response {
            status: "error".to_string(),
            content: None,
            message: Some("响应数据过大，请使用分片读取（传入 offset 和 length）".to_string()),
            size: None,
        };

        let error_json = serde_json::to_string(&error_resp)?;
        let error_bytes = error_json.as_bytes();

        let len = u32::try_from(error_bytes.len())?;
        let mut writer = BufWriter::new(std::io::stdout().lock());
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(error_bytes)?;
        writer.flush()?;
        return Ok(());
    }

    // 正常发送
    let len = u32::try_from(json_bytes.len())?;
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(json_bytes)?;
    writer.flush()?;
    Ok(())
}

// ---------- 写清单文件 ----------
fn write_manifest(manifest_path: &Path, exe_path: &str, origins: &[String]) -> Result<()> {
    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": "在用户数据目录下读写文件",
        "path": exe_path, // 直接传原始路径，由 serde 统一转义，避免双重转义
        "type": "stdio",
        "allowed_origins": origins
    });
    let mut buffer = Vec::new();
    let formatter = PrettyFormatter::with_indent(b"    ");
    let mut serializer = Serializer::with_formatter(&mut buffer, formatter);
    manifest.serialize(&mut serializer)?;
    fs::write(manifest_path, &buffer)?;
    Ok(())
}

// ---------- 从清单文件读取扩展 ID ----------
fn read_origins_from(manifest_path: &Path) -> Option<Vec<String>> {
    let data = fs::read_to_string(manifest_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&data).ok()?;
    let origins: Vec<String> = value
        .get("allowed_origins")?
        .as_array()?
        .iter()
        .filter_map(|x| x.as_str().map(String::from))
        .collect();
    if origins.is_empty() { None } else { Some(origins) }
}

// ---------- 迁移旧目录（%USERPROFILE%\.native_message）中的用户数据 ----------
fn migrate_legacy_data(install_dir: &Path, exe_filename: &str) -> Result<()> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Ok(()),
    };
    let old_dir = home.join(LEGACY_FOLDER_NAME);
    if old_dir == install_dir || !old_dir.exists() {
        return Ok(());
    }

    let manifest_name = format!("{}.json", HOST_NAME);
    let mut moved = 0usize;
    for entry in fs::read_dir(&old_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        // 跳过安装产物（EXE 与旧清单），它们由新版重新生成
        if name_str == exe_filename || name_str == manifest_name {
            continue;
        }
        let dest = install_dir.join(&name);
        if dest.exists() {
            println!("{}[跳过]{} 目标已存在: {}", YELLOW, RESET, name_str);
            continue;
        }
        match fs::rename(entry.path(), &dest) {
            Ok(_) => {
                println!(
                    "{}[迁移]{} {} -> {}",
                    GREEN,
                    RESET,
                    name_str,
                    install_dir.display()
                );
                moved += 1;
            }
            Err(e) => println!("{}[警告]{} 迁移 {} 失败：{}", YELLOW, RESET, name_str, e),
        }
    }

    // 清理旧目录中的安装残留
    let _ = fs::remove_file(old_dir.join(exe_filename));
    let _ = fs::remove_file(old_dir.join(&manifest_name));
    match fs::remove_dir(&old_dir) {
        Ok(_) => println!("{}[成功]{} 旧目录已清理: {}", GREEN, RESET, old_dir.display()),
        Err(_) => println!(
            "{}[提示]{} 旧目录中仍有文件，未删除: {}",
            BLUE,
            RESET,
            old_dir.display()
        ),
    }
    if moved > 0 {
        println!(
            "{}[成功]{} 已迁移 {} 项数据到新目录",
            GREEN,
            RESET,
            moved
        );
    }
    Ok(())
}

// ---------- 扩展 ID 校验（32 位小写字母 a-p） ----------
fn is_valid_extension_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| ('a'..='p').contains(&c))
}

// ---------- 交互式输入扩展 ID 并同步到清单 ----------
fn prompt_extension_id(manifest_path: &Path) -> Result<()> {
    println!();
    println!(
        "{}[提示]{} 请输入 Chrome 扩展 ID（32位小写字母 a-p，可在 chrome://extensions 查看）",
        BLUE, RESET
    );
    println!("{}[提示]{} 直接回车则跳过（保持清单文件中现有的 ID）", BLUE, RESET);
    print!("> ");
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => return Ok(()), // 无交互输入（如管道），静默跳过
        Ok(_) => {}
        Err(e) => {
            println!("{}[警告]{} 读取输入失败，跳过：{}", YELLOW, RESET, e);
            return Ok(());
        }
    }

    let id = line.trim().to_string();
    if id.is_empty() {
        println!(
            "{}[提示]{} 已跳过。稍后可手动编辑清单文件填写扩展 ID",
            BLUE, RESET
        );
        return Ok(());
    }
    if !is_valid_extension_id(&id) {
        println!(
            "{}[错误]{} 扩展 ID 格式无效（应为32位小写字母 a-p），清单文件未修改",
            RED, RESET
        );
        return Ok(());
    }

    let origin = format!("chrome-extension://{}/", id);
    let data = fs::read_to_string(manifest_path)?;
    let mut value: serde_json::Value = serde_json::from_str(&data)?;
    value["allowed_origins"] = serde_json::json!([origin]);
    let mut buffer = Vec::new();
    let formatter = PrettyFormatter::with_indent(b"    ");
    let mut serializer = Serializer::with_formatter(&mut buffer, formatter);
    value.serialize(&mut serializer)?;
    fs::write(manifest_path, &buffer)?;
    println!(
        "{}[成功]{} 扩展 ID 已写入清单文件: {}",
        GREEN,
        RESET,
        manifest_path.display()
    );
    Ok(())
}

// ---------- 初始化 ----------
fn init_installation() -> Result<()> {
    let install_dir = data_dir()?;
    if !install_dir.exists() {
        fs::create_dir_all(&install_dir)?;
    }
    println!("{}[成功]{} 数据目录已创建: {}", GREEN, RESET, install_dir.display());

    // 优先复用现有清单中的扩展 ID，其次旧安装，避免覆盖成占位符
    let manifest_path = install_dir.join(format!("{}.json", HOST_NAME));
    let existing_origins = read_origins_from(&manifest_path).or_else(|| {
        dirs::home_dir()
            .map(|h| h.join(LEGACY_FOLDER_NAME).join(format!("{}.json", HOST_NAME)))
            .and_then(|p| read_origins_from(&p))
    });

    let current_exe = env::current_exe()?;
    let exe_filename = current_exe
        .file_name()
        .ok_or_else(|| anyhow!("无法获取 EXE 文件名"))?
        .to_string_lossy()
        .to_string();
    let target_exe = install_dir.join(&exe_filename);
    if current_exe != target_exe {
        fs::copy(&current_exe, &target_exe)?;
    }
    println!("{}[成功]{} 软件已复制到数据目录", GREEN, RESET);

    // 迁移旧目录中的用户数据
    migrate_legacy_data(&install_dir, &exe_filename)?;

    // 写清单文件
    let exe_str = target_exe.to_string_lossy().to_string();
    let origins = existing_origins.unwrap_or_else(|| vec![PLACEHOLDER_ORIGIN.to_string()]);
    write_manifest(&manifest_path, &exe_str, &origins)?;
    println!("{}[成功]{} 已写入清单文件", GREEN, RESET);

    // 写注册表（仅 Windows）
    #[cfg(windows)]
    {
        let reg_key = format!(
            "HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\{}",
            HOST_NAME
        );
        let manifest_path_str = manifest_path.to_string_lossy();
        let status = std::process::Command::new("reg")
            .args([
                "add",
                &reg_key,
                "/ve",
                "/t",
                "REG_SZ",
                "/d",
                &manifest_path_str,
                "/f",
            ])
            .status()?;
        if status.success() {
            println!("{}[成功]{} 已创建注册表项: {}", GREEN, RESET, reg_key);
        } else {
            eprintln!("{}[错误]{} 注册表写入失败", RED, RESET);
        }
    }

    println!("{}[成功]{} 安装完成。主机名称: {}", GREEN, RESET, HOST_NAME);
    println!(
        "{}[提示]{} EXE 位置: {}",
        BLUE,
        RESET,
        target_exe.display()
    );

    // 交互式输入扩展 ID
    prompt_extension_id(&manifest_path)?;

    println!();
    println!("{}[提示]{} 重启浏览器或重新加载扩展后即可使用", BLUE, RESET);
    Ok(())
}

// ---------- 交互模式（直接运行 EXE，输入 init 安装） ----------
fn interactive_mode() -> Result<()> {
    println!();
    println!("{}NativeMessage - 浏览器插件本地存储桥{}", BLUE, RESET);
    if let Ok(dir) = data_dir() {
        println!("数据目录: {}", dir.display());
    }
    println!();
    println!("输入 {}init{} 并回车完成安装；直接关闭本窗口退出。", GREEN, RESET);
    print!("> ");
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(());
    }
    match line.trim() {
        "init" => init_installation(),
        "" => Ok(()),
        other => {
            println!(
                "{}[警告]{} 未知命令: {}（当前仅支持 init）",
                YELLOW, RESET, other
            );
            Ok(())
        }
    }
}

// ---------- 主入口 ----------
fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 && args[1] == "init" {
        return init_installation();
    }

    // stdin 是控制台 → 交互模式（用户手动运行）；是管道/文件 → Chrome 通信模式
    if stdin_is_console() {
        return interactive_mode();
    }

    // 通信模式：立即脱离控制台，销毁 Chrome 拉起时分配的黑窗口
    detach_console();

    // 设置 panic 钩子
    panic::set_hook(Box::new(|panic_info| {
        let msg = if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else {
            "未知的内部错误".to_string()
        };
        log_err(&format!("程序异常：{}", msg));
    }));

    loop {
        // 读取消息长度
        let mut len_buf = [0u8; 4];
        if std::io::stdin().read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;

        // 长度异常（如误把文本当作消息输入）时直接退出，避免巨额内存分配
        if len == 0 || len > MAX_REQUEST_SIZE {
            break;
        }

        let mut buffer = vec![0u8; len];
        if let Err(e) = std::io::stdin().read_exact(&mut buffer) {
            let resp = Response {
                status: "error".to_string(),
                content: None,
                message: Some(format!("读取请求数据失败：{}", e)),
                size: None,
            };
            let _ = send_response(&resp);
            continue;
        }

        let request_str = match String::from_utf8(buffer) {
            Ok(s) => s,
            Err(e) => {
                let resp = Response {
                    status: "error".to_string(),
                    content: None,
                    message: Some(format!("无效的 UTF-8 编码：{}", e)),
                    size: None,
                };
                let _ = send_response(&resp);
                continue;
            }
        };

        let request: Request = match serde_json::from_str(&request_str) {
            Ok(req) => req,
            Err(e) => {
                let resp = Response {
                    status: "error".to_string(),
                    content: None,
                    message: Some(format!("无效的 JSON 格式：{}", e)),
                    size: None,
                };
                let _ = send_response(&resp);
                continue;
            }
        };

        let response =
            panic::catch_unwind(|| process_request(request)).unwrap_or_else(|_| Response {
                status: "error".to_string(),
                content: None,
                message: Some("处理请求时发生内部错误".to_string()),
                size: None,
            });

        // 发送响应（内部已处理大小超限）
        if let Err(e) = send_response(&response) {
            log_err(&format!("发送响应失败：{}", e));
        }
    }
    Ok(())
}
