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
const FOLDER_NAME: &str = ".native_message";
const MAX_RESPONSE_SIZE: usize = 1024 * 1024; // 1MB

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

// ---------- 获取 exe 所在目录 ----------
fn exe_dir() -> Result<PathBuf> {
    let exe = env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("无法获取 EXE 所在目录"))?;
    Ok(dir.to_path_buf())
}

// ---------- 路径解析 ----------
fn resolve_path(raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        let base = exe_dir().unwrap_or_else(|_| PathBuf::from("."));
        base.join(raw)
    }
}

// ---------- 路径安全校验 ----------
fn is_path_allowed(requested_path: &str) -> bool {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return false,
    };
    if let Ok(canonical_target) = Path::new(requested_path).canonicalize() {
        if let Ok(canonical_home) = home.canonicalize() {
            return canonical_target.starts_with(&canonical_home);
        }
    }
    let home_str = home.to_string_lossy();
    let target_str = Path::new(requested_path).to_string_lossy();
    target_str.starts_with(&*home_str)
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
            message: Some("拒绝访问：路径必须在.native_message目录下".to_string()),
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

// ---------- 初始化 ----------
fn init_installation() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("无法获取用户主目录"))?;
    let install_dir = home.join(FOLDER_NAME);
    if !install_dir.exists() {
        fs::create_dir_all(&install_dir)?;
    }
    println!("{}[成功]{} 软件目录已创建", GREEN, RESET);

    let current_exe = env::current_exe()?;
    let exe_filename = current_exe
        .file_name()
        .ok_or_else(|| anyhow!("无法获取 EXE 文件名"))?;
    let target_exe = install_dir.join(exe_filename);
    if current_exe != target_exe {
        fs::copy(&current_exe, &target_exe)?;
    }
    println!("{}[成功]{} 软件已复制", GREEN, RESET);

    let exe_str = target_exe.to_string_lossy().replace('\\', "\\\\");

    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": "在用户主目录下读写文件",
        "path": exe_str,
        "type": "stdio",
        "allowed_origins": [
            "chrome-extension://这里填入你的Chrome插件的ID/"
        ]
    });
    let manifest_path = install_dir.join(format!("{}.json", HOST_NAME));

    let mut buffer = Vec::new();
    let formatter = PrettyFormatter::with_indent(b"    ");
    let mut serializer = Serializer::with_formatter(&mut buffer, formatter);
    manifest.serialize(&mut serializer)?;
    fs::write(&manifest_path, &buffer)?;
    println!("{}[成功]{} 已写入清单文件", GREEN, RESET);

    let reg_key = format!(
        "HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\{}",
        HOST_NAME
    );
    let manifest_path_str = manifest_path.to_string_lossy();
    let status = std::process::Command::new("reg")
        .args(&[
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

    println!("{}[成功]{} 安装完成。主机名称: {}", GREEN, RESET, HOST_NAME);
    println!(
        "{}[提示]{} EXE 已复制到: {}",
        BLUE,
        RESET,
        target_exe.display()
    );
    println!(
        "{}[提示]{} 您可以删除或移动原始 EXE 文件（不影响使用）",
        BLUE, RESET
    );
    println!(
        "\n{}[警告]{} 重要：请编辑清单文件 '{}'，将 'YOUR_EXTENSION_ID' 替换为您的扩展实际 ID",
        YELLOW,
        RESET,
        manifest_path.display()
    );
    Ok(())
}

// ---------- 主入口 ----------
fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 && args[1] == "init" {
        return init_installation();
    }

    // 设置 panic 钩子
    panic::set_hook(Box::new(|panic_info| {
        let msg = if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else {
            "未知的内部错误".to_string()
        };
        eprintln!("{}[错误]{} 程序异常：{}", RED, RESET, msg);
    }));

    loop {
        // 读取消息长度
        let mut len_buf = [0u8; 4];
        if std::io::stdin().read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;

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
            eprintln!("{}[错误]{} 发送响应失败：{}", RED, RESET, e);
        }
    }
    Ok(())
}
