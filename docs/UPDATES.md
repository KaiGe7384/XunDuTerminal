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
  "releaseUrl": "https://github.com/KaiGe7384/XunDuTerminal/releases/tag/v0.2.0"
}
```

仓库内已提供 [`deploy/xunduterminal/latest.json`](../deploy/xunduterminal/latest.json)，GitHub Raw 会直接提供该文件；发布新版本时更新版本号、说明和 Release 地址即可，无需单独部署更新服务器。

- `version`：必填，语义版本号，可带 `v` 前缀。
- `notes`：可选，关于页展示的简短更新说明。
- `releaseUrl`：可选。开源后填写 GitHub 上名为 `XunDuTerminal` 的仓库首页或 Releases 地址；客户端不会再把更新入口回退到企业级服务器网站。仓库尚未公开时请省略此字段。
- 清单超过 256 KiB、响应不是成功状态、JSON 无效或版本号无效时，客户端会显示“更新服务暂不可用”，不会影响应用其他功能。
- 更新检查只发起只读 GET 请求，不发送服务器配置、凭据或设备信息。

构建时可通过环境变量覆盖清单地址：

```powershell
$env:XUNDU_UPDATE_MANIFEST_URL='https://example.com/xunduterminal/latest.json'
npm run desktop:build
```

当前流程负责检查版本并引导用户打开官方发布页，不会静默下载或自动执行安装包。
