use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::Serializer;
use serde_json::ser::PrettyFormatter;
use std::env;
use std::fs;
use std::io::{BufWriter, Read, Write};
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

// ---------- 数据结构 ----------
#[derive(Debug, Deserialize)]
struct Request {
    file_path: String,
    content: Option<String>,
    #[serde(default = "default_encoding")]
    encoding: String,
}

fn default_encoding() -> String {
    "base64".to_string()
}

#[derive(Debug, Serialize)]
struct Response {
    status: String,
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

// ---------- 路径解析：相对路径转为 exe 目录下的绝对路径 ----------
fn resolve_path(raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        let base = exe_dir().unwrap_or_else(|_| PathBuf::from("."));
        base.join(raw)
    }
}

// ---------- 路径安全校验（仅限用户主目录） ----------
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
fn handle_read(file_path: &str, encoding: &str) -> Response {
    match fs::read(file_path) {
        Ok(data) => {
            let content_encoded = if encoding == "text" {
                match String::from_utf8(data.clone()) {
                    Ok(text) => text,
                    Err(_) => BASE64.encode(&data),
                }
            } else {
                BASE64.encode(&data)
            };
            Response {
                status: "ok".to_string(),
                content: Some(content_encoded),
                message: None,
                size: Some(data.len() as u64),
            }
        }
        Err(e) => Response {
            status: "error".to_string(),
            content: None,
            message: Some(format!("读取失败: {}", e)),
            size: None,
        },
    }
}

// ---------- 处理写入 ----------
fn handle_write(file_path: &str, content_b64: &str) -> Response {
    match BASE64.decode(content_b64) {
        Ok(data) => {
            if let Some(parent) = Path::new(file_path).parent() {
                let _ = fs::create_dir_all(parent);
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
                    message: Some(format!("写入失败: {}", e)),
                    size: None,
                },
            }
        }
        Err(e) => Response {
            status: "error".to_string(),
            content: None,
            message: Some(format!("Base64 解码失败: {}", e)),
            size: None,
        },
    }
}

// ---------- 处理请求（入口） ----------
fn handle_request(request: Request) -> Response {
    // 1. 解析路径（相对路径 → exe 目录下）
    let full_path = resolve_path(&request.file_path);
    let full_path_str = full_path.to_string_lossy().to_string();

    // 2. 安全检查：必须在用户主目录下
    if !is_path_allowed(&full_path_str) {
        return Response {
            status: "error".to_string(),
            content: None,
            message: Some("拒绝访问：路径必须在用户主目录下".to_string()),
            size: None,
        };
    }

    // 3. 根据是否有 content 字段决定读取或写入
    if let Some(content_b64) = request.content {
        handle_write(&full_path_str, &content_b64)
    } else {
        handle_read(&full_path_str, &request.encoding)
    }
}

// ---------- 发送响应 ----------
fn send_response(response: &Response) -> Result<()> {
    let json = serde_json::to_string(response)?;
    let json_bytes = json.as_bytes();
    let len =
        u32::try_from(json_bytes.len()).map_err(|_| anyhow!("响应 JSON 过大（超过 u32::MAX）"))?;

    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(json_bytes)?;
    writer.flush()?;
    Ok(())
}

// ---------- 初始化（init）----------
fn init_installation() -> Result<()> {
    // 1. 创建文件夹
    let home = dirs::home_dir().ok_or_else(|| anyhow!("无法获取用户主目录"))?;
    let install_dir = home.join(FOLDER_NAME);
    if !install_dir.exists() {
        fs::create_dir_all(&install_dir)?;
    }
    println!("{}[成功]{} 软件目录已创建", GREEN, RESET);

    // 2. 获取当前 EXE 路径并复制到安装目录
    let current_exe = env::current_exe()?;
    let exe_filename = current_exe
        .file_name()
        .ok_or_else(|| anyhow!("无法获取 EXE 文件名"))?;
    let target_exe = install_dir.join(exe_filename);

    if current_exe != target_exe {
        fs::copy(&current_exe, &target_exe)?;
    }
    println!("{}[成功]{} 软件已复制", GREEN, RESET);

    // 3. 清单中的 path 使用目标路径
    let exe_str = target_exe.to_string_lossy().replace('\\', "\\\\");

    // 4. 生成宿主清单（使用 4 空格缩进）
    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": "在用户主目录下读写文件",
        "path": exe_str,
        "type": "stdio",
        "allowed_origins": [
            "chrome-extension://YOUR_EXTENSION_ID/"
        ]
    });
    let manifest_path = install_dir.join(format!("{}.json", HOST_NAME));

    // 自定义缩进为 4 空格
    let mut buffer = Vec::new();
    let formatter = PrettyFormatter::with_indent(b"    ");
    let mut serializer = Serializer::with_formatter(&mut buffer, formatter);
    manifest.serialize(&mut serializer)?;
    fs::write(&manifest_path, &buffer)?;
    println!("{}[成功]{} 已写入清单文件", GREEN, RESET);

    // 5. 写入 Native Messaging 注册表
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

    // Native Messaging 主循环
    loop {
        let mut len_buf = [0u8; 4];
        if std::io::stdin().read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;

        let mut buffer = vec![0u8; len];
        std::io::stdin().read_exact(&mut buffer)?;
        let request_str = String::from_utf8(buffer)?;

        let request: Request = match serde_json::from_str(&request_str) {
            Ok(req) => req,
            Err(e) => {
                let resp = Response {
                    status: "error".to_string(),
                    content: None,
                    message: Some(format!("无效的 JSON: {}", e)),
                    size: None,
                };
                send_response(&resp)?;
                continue;
            }
        };

        let response = handle_request(request);
        send_response(&response)?;
    }
    Ok(())
}
