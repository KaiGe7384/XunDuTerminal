# XunDuTerminal 更新清单

“设置 → 关于 → 软件更新”默认从以下地址读取版本清单：

```text
https://raw.githubusercontent.com/KaiGe7384/XunDuTerminal/main/deploy/xunduterminal/latest.json
```

清单使用 UTF-8 JSON，示例：

```json
{
  "version": "0.2.0",
  "notes": "新增功能与修复说明。",
  "releaseUrl": "https://github.com/KaiGe7384/XunDuTerminal/releases/tag/v0.2.0",
  "publishedAt": "2026-08-01T08:00:00Z",
  "windows": {
    "url": "https://github.com/KaiGe7384/XunDuTerminal/releases/download/v0.2.0/XunDuTerminal_0.2.0_x64-setup.exe",
    "sha256": "64 位十六进制 SHA-256",
    "size": 6000000
  }
}
```

仓库内已提供 [`deploy/xunduterminal/latest.json`](../deploy/xunduterminal/latest.json)，GitHub Raw 会直接提供该文件，无需单独部署更新服务器。Release 工作流会为草稿生成 `latest.json`；正式版发布后，工作流才会把该清单同步到默认分支。预发布版本不会自动推送给普通用户。

- `version`：必填，语义版本号，可带 `v` 前缀。
- `notes`：可选，关于页展示的简短更新说明。
- `releaseUrl`：可选。开源后填写 GitHub 上名为 `XunDuTerminal` 的仓库首页或 Releases 地址；客户端不会再把更新入口回退到企业级服务器网站。仓库尚未公开时请省略此字段。
- `publishedAt`：可选，ISO 8601 发布时间。
- `windows`：可选。包含官方 NSIS `.exe` 安装包的下载地址、SHA-256 和精确字节数；缺少或校验不合法时仍可检查版本，但只提供发布页入口。
- 清单超过 256 KiB、响应不是成功状态、JSON 无效或版本号无效时，客户端会显示“更新服务暂不可用”，不会影响应用其他功能。
- 更新检查只发起只读 GET 请求，不发送服务器配置、凭据或设备信息。
- 客户端只下载 `github.com/KaiGe7384/XunDuTerminal/releases/download/...` 下、版本匹配且命名合规的 Windows NSIS 安装包，最大 512 MiB。
- 下载完成后必须同时匹配清单中的字节数和 SHA-256，随后才会允许用户手动启动安装程序；不会静默安装或自动关闭应用。

构建时可通过环境变量覆盖清单地址：

```powershell
$env:XUNDU_UPDATE_MANIFEST_URL='https://example.com/xunduterminal/latest.json'
npm run desktop:build
```

如果下载失败或被取消，可在“关于 → 软件更新”或“文件传输管理”中重新发起。已校验安装包放在应用缓存目录，重复下载同一文件时会重新校验后复用。
