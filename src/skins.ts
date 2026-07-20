export type AppAppearance = 'dark' | 'light'

export type AppThemePresetId = string

export type TerminalAnsiPalette = {
  black: string
  red: string
  green: string
  yellow: string
  blue: string
  magenta: string
  cyan: string
  white: string
  brightBlack: string
  brightRed: string
  brightGreen: string
  brightYellow: string
  brightBlue: string
  brightMagenta: string
  brightCyan: string
  brightWhite: string
}

export type GlobalThemeSettings = {
  windowBackground: string
  canvas: string
  surface: string
  elevatedSurface: string
  text: string
  mutedText: string
  accent: string
  terminalBackground: string
  terminalForeground: string
  terminalCursor: string
  terminalFontFamily: string
  terminalFontSize: number
  terminalPalette: TerminalAnsiPalette
}

export type AppThemePreset = {
  id: AppThemePresetId
  order: number
  name: string
  description: string
  badge?: string
  preview: [string, string, string]
  effects: {
    glowPrimary: string
    glowSecondary: string
    glowStrength: number
    motionIntensity: number
  }
  themes: Record<AppAppearance, GlobalThemeSettings>
}

type JsonRecord = Record<string, unknown>

const DEFAULT_PRESET_ID = 'xundu'
const DEFAULT_TERMINAL_FONT_FAMILY = '"Cascadia Code", "SF Mono", Consolas, monospace'
const DEFAULT_TERMINAL_FONT_SIZE = 12
const HEX_COLOR_PATTERN = /^#[\da-f]{6}$/i
const SKIN_ID_PATTERN = /^[a-z0-9]+(?:-[a-z0-9]+)*$/
const APPEARANCES: AppAppearance[] = ['dark', 'light']
const THEME_COLOR_KEYS = [
  'windowBackground',
  'canvas',
  'surface',
  'elevatedSurface',
  'text',
  'mutedText',
  'accent',
  'terminalBackground',
  'terminalForeground',
  'terminalCursor',
] as const
const ANSI_COLOR_KEYS = ['red', 'green', 'yellow', 'blue', 'magenta', 'cyan'] as const

const skinModules = import.meta.glob('../Skin/*/skin.json', {
  eager: true,
  import: 'default',
}) as Record<string, unknown>

function isRecord(value: unknown): value is JsonRecord {
  return Boolean(value) && typeof value === 'object' && !Array.isArray(value)
}

function readText(source: JsonRecord, key: string, context: string) {
  const value = source[key]
  if (typeof value !== 'string' || !value.trim()) {
    throw new Error(`${context}.${key} 必须是非空字符串`)
  }
  return value.trim()
}

function readHex(source: JsonRecord, key: string, context: string) {
  const value = readText(source, key, context)
  if (!HEX_COLOR_PATTERN.test(value)) {
    throw new Error(`${context}.${key} 必须是 #RRGGBB 颜色`)
  }
  return value.toLowerCase()
}

function readNumber(source: JsonRecord, key: string, context: string, minimum: number, maximum: number) {
  const value = source[key]
  if (typeof value !== 'number' || !Number.isFinite(value) || value < minimum || value > maximum) {
    throw new Error(`${context}.${key} 必须在 ${minimum} 到 ${maximum} 之间`)
  }
  return value
}

function parseHexColor(value: string) {
  const numeric = Number.parseInt(value.slice(1), 16)
  return {
    r: (numeric >> 16) & 0xff,
    g: (numeric >> 8) & 0xff,
    b: numeric & 0xff,
  }
}

function mixColor(from: string, to: string, amount: number) {
  const start = parseHexColor(from)
  const end = parseHexColor(to)
  const ratio = Math.max(0, Math.min(1, amount))
  const channel = (startValue: number, endValue: number) => Math.round(startValue + (endValue - startValue) * ratio)
  return `#${[channel(start.r, end.r), channel(start.g, end.g), channel(start.b, end.b)]
    .map((value) => value.toString(16).padStart(2, '0'))
    .join('')}`
}

function parseTheme(raw: unknown, appearance: AppAppearance, context: string): GlobalThemeSettings {
  if (!isRecord(raw)) throw new Error(`${context}.${appearance} 必须是对象`)
  const colors = Object.fromEntries(
    THEME_COLOR_KEYS.map((key) => [key, readHex(raw, key, `${context}.${appearance}`)]),
  ) as Pick<GlobalThemeSettings, typeof THEME_COLOR_KEYS[number]>
  if (!isRecord(raw.ansi)) throw new Error(`${context}.${appearance}.ansi 必须是对象`)
  const ansi = Object.fromEntries(
    ANSI_COLOR_KEYS.map((key) => [key, readHex(raw.ansi as JsonRecord, key, `${context}.${appearance}.ansi`)]),
  ) as Pick<TerminalAnsiPalette, typeof ANSI_COLOR_KEYS[number]>
  const brightTarget = appearance === 'dark' ? '#ffffff' : '#111827'
  const brighten = (color: string) => mixColor(color, brightTarget, appearance === 'dark' ? 0.2 : 0.08)

  return {
    ...colors,
    terminalFontFamily: DEFAULT_TERMINAL_FONT_FAMILY,
    terminalFontSize: DEFAULT_TERMINAL_FONT_SIZE,
    terminalPalette: {
      black: mixColor(colors.terminalBackground, colors.terminalForeground, appearance === 'dark' ? 0.14 : 0.18),
      ...ansi,
      white: mixColor(colors.terminalBackground, colors.terminalForeground, appearance === 'dark' ? 0.84 : 0.22),
      brightBlack: mixColor(colors.terminalBackground, colors.terminalForeground, 0.44),
      brightRed: brighten(ansi.red),
      brightGreen: brighten(ansi.green),
      brightYellow: brighten(ansi.yellow),
      brightBlue: brighten(ansi.blue),
      brightMagenta: brighten(ansi.magenta),
      brightCyan: brighten(ansi.cyan),
      brightWhite: appearance === 'dark'
        ? '#ffffff'
        : mixColor(colors.terminalBackground, colors.terminalForeground, 0.4),
    },
  }
}

function parseSkin(raw: unknown, source: string): AppThemePreset {
  if (!isRecord(raw)) throw new Error(`${source} 根节点必须是对象`)
  if (raw.schemaVersion !== 1) throw new Error(`${source}.schemaVersion 当前只支持 1`)
  const id = readText(raw, 'id', source)
  if (!SKIN_ID_PATTERN.test(id)) throw new Error(`${source}.id 只能使用小写字母、数字和连字符`)
  const preview = raw.preview
  if (!Array.isArray(preview) || preview.length !== 3 || preview.some((color) => typeof color !== 'string' || !HEX_COLOR_PATTERN.test(color))) {
    throw new Error(`${source}.preview 必须包含 3 个 #RRGGBB 颜色`)
  }
  if (!isRecord(raw.effects)) throw new Error(`${source}.effects 必须是对象`)
  const badge = typeof raw.badge === 'string' && raw.badge.trim() ? raw.badge.trim() : undefined

  return {
    id,
    order: readNumber(raw, 'order', source, 0, 9999),
    name: readText(raw, 'name', source),
    description: readText(raw, 'description', source),
    ...(badge ? { badge } : {}),
    preview: preview.map((color) => String(color).toLowerCase()) as [string, string, string],
    effects: {
      glowPrimary: readHex(raw.effects, 'glowPrimary', `${source}.effects`),
      glowSecondary: readHex(raw.effects, 'glowSecondary', `${source}.effects`),
      glowStrength: readNumber(raw.effects, 'glowStrength', `${source}.effects`, 0, 0.3),
      motionIntensity: readNumber(raw.effects, 'motionIntensity', `${source}.effects`, 0, 1),
    },
    themes: Object.fromEntries(
      APPEARANCES.map((appearance) => [appearance, parseTheme(raw[appearance], appearance, source)]),
    ) as Record<AppAppearance, GlobalThemeSettings>,
  }
}

function createEmergencyPreset(): AppThemePreset {
  const dark = {
    windowBackground: '#101114', canvas: '#17181c', surface: '#202126', elevatedSurface: '#2a2b31',
    text: '#f2f3f5', mutedText: '#9da1aa', accent: '#62a8ff', terminalBackground: '#111216',
    terminalForeground: '#e8ebf0', terminalCursor: '#86b9ff',
    ansi: { red: '#ff6b6b', green: '#44c78a', yellow: '#e6b450', blue: '#62a8ff', magenta: '#c792ea', cyan: '#56cfe1' },
  }
  const light = {
    windowBackground: '#e9eaed', canvas: '#f3f4f6', surface: '#fafafc', elevatedSurface: '#ffffff',
    text: '#202226', mutedText: '#6f747d', accent: '#1769e0', terminalBackground: '#fbfbfc',
    terminalForeground: '#25282e', terminalCursor: '#1769e0',
    ansi: { red: '#c93636', green: '#18864b', yellow: '#9a6700', blue: '#1769e0', magenta: '#8f3cb6', cyan: '#0b7f8c' },
  }
  return parseSkin({
    schemaVersion: 1,
    id: DEFAULT_PRESET_ID,
    order: 0,
    name: 'XunDu 默认',
    description: '中性石墨工作台，克制、清晰，适合长时间运维。',
    badge: '默认',
    preview: ['#17181c', '#202126', '#62a8ff'],
    effects: { glowPrimary: '#62a8ff', glowSecondary: '#8cbcff', glowStrength: 0.05, motionIntensity: 0.55 },
    dark,
    light,
  }, '内置应急主题')
}

const loadedPresets: AppThemePreset[] = []
const loadedIds = new Set<string>()
for (const [source, raw] of Object.entries(skinModules).sort(([left], [right]) => left.localeCompare(right))) {
  try {
    const preset = parseSkin(raw, source)
    if (loadedIds.has(preset.id)) {
      console.warn(`[Skin] 已忽略重复主题 ID：${preset.id}（${source}）`)
      continue
    }
    loadedIds.add(preset.id)
    loadedPresets.push(preset)
  } catch (error) {
    console.warn(`[Skin] 已忽略无效主题 ${source}：${String(error)}`)
  }
}

if (!loadedIds.has(DEFAULT_PRESET_ID)) loadedPresets.push(createEmergencyPreset())

export const appThemePresets = loadedPresets.sort((left, right) => left.order - right.order || left.name.localeCompare(right.name))

export const appThemePresetMap = Object.fromEntries(
  appThemePresets.map((preset) => [preset.id, preset]),
) as Record<AppThemePresetId, AppThemePreset>

export const DEFAULT_THEME_PRESET_ID = DEFAULT_PRESET_ID

export function isAppThemePresetId(value: unknown): value is AppThemePresetId {
  return typeof value === 'string' && Object.hasOwn(appThemePresetMap, value)
}
