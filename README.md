# NativeMessage - 浏览器插件本地存储桥

## 📖 项目简介

`NativeMessage` 是一个用 Rust 编写的 **Chrome 扩展 Native Messaging 主机**，为浏览器扩展提供安全、高效的**本地文件读写能力**。通过本工具，扩展可以在用户主目录下的 `.native_message` 文件夹中安全地读写文件（包括文本、图片、Excel 等二进制文件），并支持**大文件自动分片读取**，完美规避 Chrome 的消息大小限制。

---

## ✨ 功能特性

- ✅ **双向文件操作**：支持读取和写入本地文件。
- ✅ **自动分片读取**：`readFileWhole` 自动处理大文件，无需手动分片。
- ✅ **手动分片控制**：`readFile` 支持指定 `offset`/`length`，灵活应对各种场景。
- ✅ **Base64 传输**：所有文件内容以 Base64 格式传输，完美兼容二进制数据。
- ✅ **固定安全目录**：所有文件操作严格限制在 `%USERPROFILE%/.native_message` 目录下，避免越权访问。
- ✅ **一键安装部署**：`init` 命令自动完成所有安装步骤。
- ✅ **响应大小保护**：自动拒绝发送超过 1MB 的响应，避免通信崩溃。
- ✅ **中文错误提示**：所有错误信息以中文返回给扩展，便于调试。

---

## 🖥️ 系统要求

- **Rust 工具链**（≥ 1.85.0）
- **Chrome / Edge / Chromium 内核浏览器**
- 支持 Native Messaging API 的扩展环境

---

## 🚀 安装与部署

### 1. 编译 或 直接下载成品exe

```bash
cargo build --release
```

编译产物位于 `target/release/NativeMessage.exe`（Windows）或 `NativeMessage`（Linux/macOS）。

### 2. 安装（运行 `init`）

将可执行文件放置到任意固定位置，然后执行：

```bash
# Windows
NativeMessage.exe init

# Linux/macOS
./NativeMessage init
```

该命令自动完成：
- 在用户主目录创建 `.native_message` 文件夹
- 将自身复制到该文件夹
- 生成 `top.hugstars.native_message.json` 清单文件
- 写入注册表（Windows）或配置文件（macOS/Linux 需手动配置）

### 3. 修改扩展 ID

编辑 `~/.native_message/top.hugstars.native_message.json`，将 `allowed_origins` 中的 `这里填入你的Chrome插件的ID` 替换为您的扩展实际 ID（在 `chrome://extensions` 中查看）。

### 4. 重启浏览器或重新加载扩展

---

## 📦 扩展端使用说明

在您的 Chrome 扩展中，首先在 `manifest.json` 中添加权限：

```json
"permissions": ["nativeMessaging"]
```

然后复制以下完整代码到您的扩展中（例如 `background.js` 或 `popup.js`）：

```javascript
// ============================================================
// 配置
// ============================================================
const CHUNK_SIZE = 512 * 1024  // 每块 512KB
const HOST_NAME = "top.hugstars.native_message"

// ============================================================
// 1. 手动分片读取（支持指定 offset/length）
// ============================================================
function readFile(filePath, offset, length) {
    let data = { file_path: filePath }
    if (offset !== undefined) data.offset = offset
    if (length !== undefined) data.length = length

    console.log(data)
    return new Promise((resolve, reject) => {
        chrome.runtime.sendNativeMessage(
            HOST_NAME,
            data,
            (response) => {
                console.log(response)
                if (chrome.runtime.lastError) return reject(new Error(chrome.runtime.lastError.message))
                if (response.status === "ok") resolve(response.content)
                else reject(new Error(response.message || "Unknown error"))
            }
        )
    })
}

// ============================================================
// 2. 辅助转换函数
// ============================================================
// Base64 → Uint8Array
function base64ToUint8Array(base64) {
    const binary = atob(base64)
    const bytes = new Uint8Array(binary.length)
    for (let i = 0; i < binary.length; i++) {
        bytes[i] = binary.charCodeAt(i)
    }
    return bytes
}

// Uint8Array → Base64
function uint8ArrayToBase64(uint8Array) {
    let binary = ''
    for (let i = 0; i < uint8Array.length; i++) {
        binary += String.fromCharCode(uint8Array[i])
    }
    return btoa(binary)
}

// ============================================================
// 3. 获取文件大小（利用 length=0）
// ============================================================
function getFileSize(filePath) {
    return new Promise((resolve, reject) => {
        chrome.runtime.sendNativeMessage(
            HOST_NAME,
            { file_path: filePath, offset: 0, length: 0 },
            (response) => {
                if (chrome.runtime.lastError) {
                    reject(new Error(chrome.runtime.lastError.message))
                    return
                }
                if (response.status === "ok") return resolve(response.size || 0)
                reject(new Error(response.message || "获取大小失败"))
            }
        )
    })
}

// ============================================================
// 4. 自动分片读取完整文件（推荐）
// ============================================================
async function readFileWhole(filePath, chunkSize = CHUNK_SIZE) {
    // 1. 获取文件总大小
    const totalSize = await getFileSize(filePath)

    // 2. 如果文件很小，直接读取
    if (totalSize <= chunkSize) {
        return await readFile(filePath, 0, totalSize)
    }

    // 3. 分片读取
    let allChunks = []
    let offset = 0
    while (offset < totalSize) {
        const length = Math.min(chunkSize, totalSize - offset)
        const chunkBase64 = await readFile(filePath, offset, length)
        allChunks.push(base64ToUint8Array(chunkBase64))
        offset += length
    }

    // 4. 合并所有 Uint8Array
    const totalLength = allChunks.reduce((sum, arr) => sum + arr.length, 0)
    const merged = new Uint8Array(totalLength)
    let pos = 0
    for (const arr of allChunks) {
        merged.set(arr, pos)
        pos += arr.length
    }

    // 5. 整体转 Base64 返回
    return uint8ArrayToBase64(merged)
}

// ============================================================
// 5. 写入文件
// ============================================================
function writeFile(filePath, base64Data) {
    return new Promise((resolve, reject) => {
        chrome.runtime.sendNativeMessage(
            HOST_NAME,
            { file_path: filePath, content: base64Data },
            (response) => {
                if (chrome.runtime.lastError) {
                    reject(new Error(chrome.runtime.lastError.message))
                    return
                }
                if (response.status === "ok") return resolve(response)
                reject(new Error(response.message || "写入失败"))
            }
        )
    })
}
```

---

### 📂 文件存储位置（重要）

**当前版本仅支持读写 `%USERPROFILE%\.native_message` 目录下的文件**，这是为了确保安全和统一管理。

- 您在使用 `readFile` 或 `writeFile` 时，**无论传入的是绝对路径还是相对路径**，最终都会映射到 `.native_message` 目录。
- 因此，**强烈建议直接使用文件名（相对路径）** 来操作文件，例如：

```javascript
// ✅ 推荐：直接使用文件名
await readFile("config.json")        // 实际读取 C:/Users/用户名/.native_message/config.json
await writeFile("data.db", base64)   // 写入 C:/Users/用户名/.native_message/data.db

// ⚠️ 不推荐：绝对路径也会被重定向到 .native_message
await readFile("C:/Users/用户名/Desktop/photo.jpg")  
// 实际仍会尝试读取 C:/Users/用户名/.native_message/photo.jpg（如果不存在则报错）
```

> **注意**：如果您传入的绝对路径指向 `.native_message` 以外的位置，主机可能会拒绝访问或找不到文件。请始终使用文件名操作。

---

### 📂 如何管理文件

您可以在系统文件管理器中直接访问该目录：

- Windows：`C:\Users\你的用户名\.native_message`
- macOS/Linux：`/Users/你的用户名/.native_message`

您也可以手动创建子文件夹来组织文件，例如：

```javascript
await writeFile("images/avatar.png", base64)  // 写入 .native_message/images/avatar.png
```

---

## 🧪 使用示例

#### 读取文本文件

```javascript
// 自动分片读取（推荐）
const base64 = await readFileWhole("test.txt")
// 解码为文本
const bytes = base64ToUint8Array(base64)
const text = new TextDecoder('utf-8').decode(bytes)
console.log(text)

// 手动分片（只读前 100 字节）
const chunk = await readFile("test.txt", 0, 100)
// chunk 是 Base64 片段
```

#### 读取图片并显示

```javascript
const base64 = await readFileWhole("photo.jpg")
const blob = new Blob([base64ToUint8Array(base64)], { type: "image/jpeg" })
const url = URL.createObjectURL(blob)
document.getElementById("myImg").src = url
```

#### 写入文本文件

```javascript
// 将字符串转为 Base64
const text = "Hello, 世界！"
const encoder = new TextEncoder()
const data = encoder.encode(text)
let binary = ''
data.forEach(byte => binary += String.fromCharCode(byte))
const base64 = btoa(binary)

await writeFile("output.txt", base64)
console.log("写入成功")
```

#### 写入图片（从 Canvas）

```javascript
const canvas = document.getElementById("canvas")
const dataUrl = canvas.toDataURL("image/png")
const base64 = dataUrl.split(',')[1]
await writeFile("canvas.png", base64)
```

#### 写入 Excel 文件（使用 SheetJS）

```javascript
// 引入 SheetJS 库后
const workbook = XLSX.utils.book_new()
const data = [["姓名", "年龄"], ["张三", 28]]
const ws = XLSX.utils.aoa_to_sheet(data)
XLSX.utils.book_append_sheet(workbook, ws, "Sheet1")
const wbout = XLSX.write(workbook, { bookType: 'xlsx', type: 'array' })
const bytes = new Uint8Array(wbout)
let binary = ''
bytes.forEach(byte => binary += String.fromCharCode(byte))
const base64 = btoa(binary)
await writeFile("data.xlsx", base64)
```

---

## 🔒 安全性说明

- **路径限制**：所有操作均限定在 `%USERPROFILE%/.native_message` 目录下，防止越权访问。
- **响应大小保护**：响应 JSON 超过 1MB 时，自动返回错误信息，防止进程崩溃。
- **扩展白名单**：清单文件中的 `allowed_origins` 仅允许指定扩展 ID 访问。

---

## 📝 通信协议

- **消息格式**：`[4字节小端长度][UTF-8 JSON]`
- **请求字段**：
  - `file_path` (string)：文件路径（相对路径以 `.native_message` 为基准）
  - `content` (string, 可选)：写入时提供 Base64 数据
  - `offset` (u64, 可选)：读取起始偏移（字节）
  - `length` (u64, 可选)：读取长度（字节）
- **响应字段**：
  - `status` (string)：`"ok"` 或 `"error"`
  - `content` (string, 可选)：读取时返回 Base64 数据
  - `message` (string, 可选)：错误或提示信息
  - `size` (u64, 可选)：文件总字节数

---

## ⚠️ 注意事项

- **消息大小限制**：单条消息最大约 1MB，读取大文件请使用 `readFileWhole` 自动分片。
- **Base64 开销**：Base64 使体积增加约 33%，分片大小时需考虑。
- **路径格式**：建议使用文件名（相对路径），避免使用绝对路径。
- **权限**：主机以用户级权限运行，只能访问当前用户有权限的文件（`.native_message` 目录位于用户主目录下，权限充足）。

---

## 📄 许可证

本项目采用 MIT 许可证，详见 [LICENSE](./LICENSE) 文件。

---

## 🤝 贡献与反馈

欢迎提交 Issue 和 Pull Request，共同完善本项目。
