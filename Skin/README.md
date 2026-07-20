# XunDuTerminal Skin 主题预设

`Skin/` 中每个一级子目录代表一套主题，应用会在构建时自动发现其中的 `skin.json`。复制任意现有目录、修改 `id` 和颜色后重新构建，即可加入自己的预设；不需要修改 React 或 CSS。

## 最短制作流程

1. 复制 `Skin/xundu-default/`，例如改名为 `Skin/my-skin/`。
2. 修改 `skin.json` 中的 `id`、`name`、`description`、`order` 与颜色。
3. 保持 `id` 仅含小写字母、数字和连字符，并确保不与其他主题重复。
4. 在项目根目录运行 `npm run build`；无效文件会被忽略并在控制台给出原因，默认主题仍可安全启动。

## 字段说明

- `schemaVersion`：当前固定为 `1`。
- `id`：主题唯一 ID，也是本地持久化值；发布后不建议修改。
- `order`：设置页中的排序，范围 `0–9999`。
- `name` / `description`：主题卡片文字；可直接使用中文或英文。
- `badge`：可选短标签。建议只用于“默认”等必要状态，不写来源介绍。
- `preview`：卡片缩略图的画布、面板、强调色，共 3 个 `#RRGGBB` 颜色。
- `effects.glowPrimary` / `glowSecondary`：工作台氛围光和切换过渡色。
- `effects.glowStrength`：静态氛围光强度，范围 `0–0.3`。
- `effects.motionIntensity`：主题切换动效强度，范围 `0–1`；系统“减少动态效果”开启时会自动停用位移动效。
- `dark` / `light`：深色和浅色的完整界面、终端基础配色。
- `ansi`：终端基础六色；亮色 ANSI 会按当前亮暗模式自动生成，保持一致的对比度。

完整机器可读规则见 [`schema.json`](./schema.json)。皮肤文件只允许 JSON 数据，不执行脚本，也不能访问应用凭据。

## 设计建议

- 深色和浅色都要单独校对文本、占位符、悬停和选中状态，不能只把颜色整体反转。
- `text` 与 `canvas`、`mutedText` 与 `surface` 应保持足够对比度。
- `terminalBackground`、`terminalForeground` 和六个 ANSI 色要在真实命令输出中检查。
- 氛围光建议克制使用；主题识别应主要来自配色层级，而不是持续闪烁或大面积发光。
