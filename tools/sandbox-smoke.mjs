import { spawn, spawnSync } from 'node:child_process'
import { writeFile } from 'node:fs/promises'
import { chromium } from 'playwright'

const port = Number(process.env.XUNDU_SANDBOX_PORT ?? 5174)
const url = `http://127.0.0.1:${port}/`
const npmCmd = process.platform === 'win32' ? 'npm.cmd' : 'npm'
const serverCommand = process.platform === 'win32' ? (process.env.ComSpec ?? 'cmd.exe') : npmCmd
const serverArgs = process.platform === 'win32'
  ? ['/d', '/s', '/c', `${npmCmd} run dev:sandbox -- --port ${port} --strictPort`]
  : ['run', 'dev:sandbox', '--', '--port', String(port), '--strictPort']

const server = spawn(
  serverCommand,
  serverArgs,
  { cwd: process.cwd(), stdio: ['ignore', 'pipe', 'pipe'] },
)

let serverLog = ''
server.stdout.on('data', (chunk) => {
  serverLog += chunk.toString()
})
server.stderr.on('data', (chunk) => {
  serverLog += chunk.toString()
})

try {
  await waitForServer(url)

  const browser = await launchBrowser()
  const page = await browser.newPage({
    viewport: { width: 1482, height: 922 },
    deviceScaleFactor: 1,
    locale: 'zh-CN',
  })
  await page.context().grantPermissions(['clipboard-read', 'clipboard-write'], { origin: new URL(url).origin })
  const pageErrors = []
  page.on('console', (message) => {
    if (message.type() === 'error') pageErrors.push(message.text())
  })
  page.on('pageerror', (error) => pageErrors.push(String(error)))

  await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 90_000 })
  await page.waitForSelector('.app-shell', { timeout: 15_000 })
  await page.waitForTimeout(200)
  await page.evaluate(() => {
    localStorage.clear()
    sessionStorage.removeItem('__xundu.sandbox.credentials')
    localStorage.setItem('xundu.servers', JSON.stringify([
      { id: 'sandbox-198', name: '192.0.2.198', host: '192.0.2.198', user: 'root', port: 22, group: 'Sandbox', auth: 'Password', password: 'sandbox' },
      { id: 'sandbox-201', name: '192.0.2.201', host: '192.0.2.201', user: 'root', port: 22, group: 'Sandbox', auth: 'Password', password: 'sandbox' },
      { id: 'sandbox-81', name: '192.0.2.81', host: '192.0.2.81', user: 'root', port: 22, group: 'Sandbox', auth: 'Password', password: 'sandbox' },
      { id: 'sandbox-91', name: '192.0.2.91', host: '192.0.2.91', user: 'root', port: 22, group: 'Sandbox', auth: 'Password', password: 'sandbox' },
    ]))
    localStorage.setItem('xundu.phase2.session.v1', '{"status":"running"}')
    localStorage.setItem('xundu.phase2.recovery.v1', '[{"legacy":true}]')
    localStorage.setItem('__xundu.sandbox.recovery', '[{"legacy":true}]')
  })
  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.waitForSelector('.app-shell', { timeout: 15_000 })
  await page.waitForTimeout(800)

  const migratedCredentialState = await page.evaluate(() => ({
    servers: JSON.parse(localStorage.getItem('xundu.servers') ?? '[]'),
    vault: JSON.parse(sessionStorage.getItem('__xundu.sandbox.credentials') ?? '{}'),
  }))
  assert(
    migratedCredentialState.servers.every((server) => !Object.hasOwn(server, 'password')),
    'legacy SSH passwords remained in localStorage after credential migration',
  )
  assert(
    Object.keys(migratedCredentialState.vault).filter((key) => key.startsWith('ssh:')).length === 4,
    'legacy SSH passwords were not migrated into the credential vault',
  )
  const removedRecoveryState = await page.evaluate(() => ({
    session: localStorage.getItem('xundu.phase2.session.v1'),
    snapshots: localStorage.getItem('xundu.phase2.recovery.v1'),
    sandbox: localStorage.getItem('__xundu.sandbox.recovery'),
  }))
  assert(Object.values(removedRecoveryState).every((value) => value === null), `deprecated recovery state was not removed: ${JSON.stringify(removedRecoveryState)}`)
  assert(await page.locator('.session-recovery-banner').count() === 0, 'removed recovery banner is still rendered')
  assert(await page.evaluate(() => (window.__XUNDU_SANDBOX_SSH_CONNECTS__ ?? 0) === 0), 'SSH connected during application startup')
  assert(await page.evaluate(() => (window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0) === 0), 'RDP connected during application startup')

  const defaultAppearance = await page.evaluate(() => document.documentElement.dataset.appearance)
  assert(defaultAppearance === 'dark', `default appearance should be dark, got ${defaultAppearance}`)
  assert(await page.evaluate(() => document.documentElement.dataset.themePreset === 'xundu'), 'XunDu was not the default theme preset')
  assert(await page.locator('.app-custom-background').count() === 0, 'custom background was enabled by default')
  assert(await page.evaluate(() => document.documentElement.dataset.customBackground === 'disabled'), 'default background state was not disabled')

  await page.locator('.titlebar button[aria-label="设置"]').click()
  const backgroundSwitch = page.getByRole('switch', { name: '启用自定义背景' })
  assert(await backgroundSwitch.getAttribute('aria-checked') === 'false', 'background switch started enabled')
  const themePresetCards = page.locator('.theme-preset-card')
  assert(await themePresetCards.count() === 8, 'settings did not render all eight file-backed theme presets')
  assert(await themePresetCards.filter({ hasText: 'XunDu 默认' }).getAttribute('aria-checked') === 'true', 'XunDu default preset was not selected')
  const presetExpectations = [
    ['XunDu 默认', 'xundu', '#62a8ff', '#111216'],
    ['Dream Skin', 'dream-skin', '#d66f82', '#151217'],
    ['Aurora Glass', 'aurora', '#35c6b2', '#07191c'],
    ['Ember Noir', 'ember', '#e59652', '#16120e'],
    ['Paper Frost', 'paper-frost', '#b07a3f', '#181815'],
    ['Midnight Ink', 'midnight-ink', '#4da3ff', '#070d18'],
    ['Sakura Mist', 'sakura-mist', '#e384a5', '#161217'],
    ['Forest Signal', 'forest-signal', '#55b779', '#0c150f'],
  ]
  const discoveredSkinIds = await themePresetCards.evaluateAll((cards) => cards.map((card) => card.getAttribute('data-skin-id')))
  assert(
    discoveredSkinIds.join(',') === presetExpectations.map(([, id]) => id).join(','),
    `file-backed skin discovery order was incorrect: ${JSON.stringify(discoveredSkinIds)}`,
  )
  for (const [label, id, accent, terminal] of presetExpectations) {
    await themePresetCards.filter({ hasText: label }).click()
    await page.waitForFunction((expected) => document.documentElement.dataset.themePreset === expected, id)
    const presetState = await page.evaluate(() => {
      const style = getComputedStyle(document.documentElement)
      return {
        stored: localStorage.getItem('xundu.themePreset'),
        accent: style.getPropertyValue('--accent').trim(),
        terminal: style.getPropertyValue('--terminal-bg').trim(),
      }
    })
    assert(presetState.stored === id, `${label} preset was not persisted: ${JSON.stringify(presetState)}`)
    assert(presetState.accent === accent, `${label} accent was not applied: ${JSON.stringify(presetState)}`)
    assert(presetState.terminal === terminal, `${label} terminal palette was not applied: ${JSON.stringify(presetState)}`)
  }
  const presetOverflow = await page.evaluate(() => [...document.querySelectorAll('.theme-preset-card')]
    .filter((card) => card.scrollWidth > card.clientWidth + 1 || card.scrollHeight > card.clientHeight + 1)
    .map((card) => card.textContent?.trim()))
  assert(presetOverflow.length === 0, `theme preset cards overflowed: ${JSON.stringify(presetOverflow)}`)
  const activeSweepAnimation = await page.evaluate(() => {
    const activeCard = document.querySelector('.theme-preset-card.active')
    return activeCard ? getComputedStyle(activeCard, '::after').animationName : ''
  })
  assert(activeSweepAnimation === 'theme-preset-edge-sweep', `selected theme did not expose its one-shot edge sweep: ${activeSweepAnimation}`)
  await page.emulateMedia({ reducedMotion: 'reduce' })
  const reducedSweepAnimation = await page.evaluate(() => {
    const activeCard = document.querySelector('.theme-preset-card.active')
    return activeCard ? getComputedStyle(activeCard, '::after').animationName : ''
  })
  assert(reducedSweepAnimation === 'none', `reduced motion did not disable the theme edge sweep: ${reducedSweepAnimation}`)
  await page.emulateMedia({ reducedMotion: 'no-preference' })
  await themePresetCards.filter({ hasText: 'Dream Skin' }).click()
  await page.waitForFunction(() => document.documentElement.dataset.themePreset === 'dream-skin')
  await page.waitForTimeout(500)
  await page.screenshot({ path: 'output/playwright/theme-presets-file-backed.png', fullPage: false })
  await page.locator('.settings-nav button').filter({ hasText: '关于' }).click()
  const aboutText = await page.locator('.settings-pane').innerText()
  assert(!/Codex Dream Skin|MIT|开源项目/.test(aboutText), `about section still exposed theme source copy: ${aboutText}`)
  assert(aboutText.includes('XunDuTerminal') && aboutText.includes('v0.1.0'), 'about section did not show product identity and version')
  assert(aboutText.includes('https://xunduyun.com/'), 'about section did not show the enterprise server website')
  assert(aboutText.includes('1090339570') && aboutText.includes('262430517'), 'about section did not show both technical QQ groups')
  assert(!aboutText.includes('前往官网'), 'update section still exposed the retired website fallback')
  const aboutTypography = await page.evaluate(() => {
    const fontSize = (selector) => {
      const element = document.querySelector(selector)
      return element ? Number.parseFloat(getComputedStyle(element).fontSize) : 0
    }
    return {
      title: fontSize('.about-product-title h3'),
      description: fontSize('.about-product-copy p'),
      cardTitle: fontSize('.about-card-heading strong'),
      groupNumber: fontSize('.about-community-card > button strong'),
    }
  })
  assert(aboutTypography.title >= 20, `about product title remained too small: ${aboutTypography.title}px`)
  assert(aboutTypography.description >= 13, `about description remained too small: ${aboutTypography.description}px`)
  assert(aboutTypography.cardTitle >= 13, `about card heading remained too small: ${aboutTypography.cardTitle}px`)
  assert(aboutTypography.groupNumber >= 12, `about QQ group number remained too small: ${aboutTypography.groupNumber}px`)
  await page.getByRole('button', { name: /检查更新/ }).click()
  await page.getByText('当前已是最新版本。').waitFor()
  await page.locator('.about-resource-card.website').click()
  await page.waitForFunction(() => (window.__XUNDU_SANDBOX_EXTERNAL_URLS__ ?? []).includes('https://xunduyun.com/'))
  assert(
    await page.evaluate(() => (window.__XUNDU_SANDBOX_EXTERNAL_URLS__ ?? []).includes('https://xunduyun.com/')),
    'enterprise server website did not use the safe external-link bridge',
  )
  await page.locator('.about-community-card > button').first().click()
  await page.getByText('正在跳转').waitFor()
  const firstQqGroupUrl = 'mqqapi://card/show_pslcard?src_type=internal&version=1&uin=1090339570&card_type=group&source=qrcode'
  await page.waitForFunction((url) => (window.__XUNDU_SANDBOX_EXTERNAL_URLS__ ?? []).includes(url), firstQqGroupUrl)
  assert(
    await page.evaluate((url) => (window.__XUNDU_SANDBOX_EXTERNAL_URLS__ ?? []).includes(url), firstQqGroupUrl),
    'technical QQ group did not use the safe external-link bridge',
  )
  const aboutOverflow = await page.evaluate(() => {
    const about = document.querySelector('.about-page')
    return about ? about.scrollWidth > about.clientWidth + 1 : true
  })
  assert(!aboutOverflow, 'about software, update, and community content overflowed horizontally')
  await page.screenshot({ path: 'output/playwright/about-software-and-updates.png', fullPage: false })
  await page.locator('.settings-modal button[aria-label="关闭"]').click()
  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.waitForSelector('.app-shell')
  assert(await page.evaluate(() => document.documentElement.dataset.themePreset === 'dream-skin'), 'Dream Skin preset did not survive an application reload')
  await page.locator('.titlebar button[aria-label="设置"]').click()
  await themePresetCards.filter({ hasText: 'XunDu 默认' }).click()
  await page.locator('.settings-appearance-switch button').first().click()
  const appearanceLayout = await page.evaluate(() => {
    const copy = document.querySelector('.app-appearance-row > div:first-child')?.getBoundingClientRect()
    const control = document.querySelector('.settings-appearance-switch')?.getBoundingClientRect()
    const titlebar = document.querySelector('.titlebar')
    const terminalHost = document.querySelector('.widget-terminal-host')
    const titlebarStyle = titlebar ? getComputedStyle(titlebar) : null
    const terminalStyle = terminalHost ? getComputedStyle(terminalHost) : null
    return {
      appearance: document.documentElement.dataset.appearance,
      clearGap: Boolean(copy && control && copy.right + 8 <= control.left),
      terminalBackgroundToken: getComputedStyle(document.documentElement).getPropertyValue('--terminal-bg').trim(),
      terminalBackground: terminalStyle?.backgroundColor ?? '',
      titlebarBackground: titlebarStyle?.backgroundColor ?? '',
      titlebarFilter: titlebarStyle?.backdropFilter ?? '',
    }
  })
  assert(appearanceLayout.appearance === 'light', 'settings did not switch to the light appearance')
  assert(appearanceLayout.clearGap, 'settings appearance copy overlaps the segmented control')
  assert(appearanceLayout.terminalBackgroundToken === '#fbfbfc', `light terminal token did not follow appearance: ${appearanceLayout.terminalBackgroundToken}`)
  assert(appearanceLayout.terminalBackground === 'rgb(251, 251, 252)', `light terminal host stayed dark: ${appearanceLayout.terminalBackground}`)
  assert(appearanceLayout.titlebarBackground === 'rgb(250, 250, 252)', `titlebar is not opaque: ${appearanceLayout.titlebarBackground}`)
  assert(appearanceLayout.titlebarFilter === 'none', `titlebar still applies a backdrop filter: ${appearanceLayout.titlebarFilter}`)
  const terminalFontSlider = page.locator('.terminal-font-size-control input[type="range"]')
  assert(await terminalFontSlider.inputValue() === '12', 'terminal font size did not use the compact default')
  await terminalFontSlider.fill('11')
  await page.waitForTimeout(180)
  const terminalFontState = await page.evaluate(() => ({
    stored: localStorage.getItem('xundu.terminalFontSize'),
    rendered: getComputedStyle(document.querySelector('.xterm-char-measure-element')).fontSize,
  }))
  assert(terminalFontState.stored === '11', `terminal font size was not persisted: ${terminalFontState.stored}`)
  assert(terminalFontState.rendered === '11px', `terminal font size did not update xterm: ${terminalFontState.rendered}`)
  await page.screenshot({ path: 'output/playwright/theme-light-solid.png', fullPage: false })

  await page.getByRole('button', { name: '选择图片' }).click()
  await page.locator('.app-custom-background').waitFor({ state: 'visible' })
  assert(await backgroundSwitch.getAttribute('aria-checked') === 'true', 'choosing a background did not enable it')
  const transparencySlider = page.getByRole('slider', { name: '整体透明度' })
  assert(await transparencySlider.inputValue() === '18', 'custom background did not use the balanced default transparency')
  await transparencySlider.fill('34')
  await page.waitForTimeout(180)
  const backgroundState = await page.evaluate(() => {
    const stored = JSON.parse(localStorage.getItem('xundu.appBackground') ?? '{}')
    const style = getComputedStyle(document.documentElement)
    const titlebar = document.querySelector('.titlebar')
    return {
      stored,
      dataset: document.documentElement.dataset.customBackground,
      transparency: document.documentElement.dataset.interfaceTransparency,
      surface: style.getPropertyValue('--surface-0').trim(),
      terminal: style.getPropertyValue('--terminal-bg').trim(),
      titlebarFilter: titlebar ? getComputedStyle(titlebar).backdropFilter : '',
      selections: window.__XUNDU_SANDBOX_BACKGROUND_SELECTIONS__ ?? 0,
    }
  })
  assert(backgroundState.stored.enabled === true && backgroundState.stored.transparency === 34, `background settings were not persisted: ${JSON.stringify(backgroundState)}`)
  assert(backgroundState.dataset === 'enabled' && backgroundState.transparency === '34', `background dataset was not applied: ${JSON.stringify(backgroundState)}`)
  assert(backgroundState.surface.startsWith('rgba('), `workspace surface did not become translucent: ${backgroundState.surface}`)
  assert(backgroundState.terminal.startsWith('rgba('), `terminal surface did not become translucent: ${backgroundState.terminal}`)
  assert(backgroundState.titlebarFilter.includes('blur'), `background mode did not enable material blur: ${backgroundState.titlebarFilter}`)
  assert(backgroundState.selections === 1, 'background picker did not run exactly once')
  await page.screenshot({ path: 'output/playwright/settings-custom-background.png', fullPage: false })

  const firstBackgroundPath = backgroundState.stored.path
  await page.getByRole('button', { name: '更换图片' }).click()
  await page.waitForFunction(() => (window.__XUNDU_SANDBOX_BACKGROUND_SELECTIONS__ ?? 0) === 2)
  const replacementBackgroundPath = await page.evaluate(() => JSON.parse(localStorage.getItem('xundu.appBackground') ?? '{}').path)
  assert(replacementBackgroundPath !== firstBackgroundPath, 'replacing a background reused the old asset URL and could keep a stale cached image')

  await backgroundSwitch.click()
  await page.locator('.app-custom-background').waitFor({ state: 'detached' })
  assert(await transparencySlider.isDisabled(), 'transparency control stayed enabled while the background was off')
  assert(await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--surface-0').trim() === '#f3f4f6'), 'disabling the background did not restore solid theme tokens')
  await backgroundSwitch.click()
  await page.locator('.app-custom-background').waitFor({ state: 'visible' })
  assert(await page.evaluate(() => window.__XUNDU_SANDBOX_BACKGROUND_SELECTIONS__ === 2), 're-enabling a saved background reopened the picker')
  await page.locator('.settings-modal button[aria-label="关闭"]').click()
  await page.waitForTimeout(250)
  await page.screenshot({ path: 'output/playwright/workspace-custom-background.png', fullPage: false })

  await page.locator('.titlebar button[aria-label="设置"]').click()
  await page.getByRole('button', { name: '清除' }).click()
  await page.locator('.app-custom-background').waitFor({ state: 'detached' })
  const clearedBackground = await page.evaluate(() => ({
    stored: JSON.parse(localStorage.getItem('xundu.appBackground') ?? '{}'),
    clears: window.__XUNDU_SANDBOX_BACKGROUND_CLEARS__ ?? 0,
  }))
  assert(clearedBackground.clears === 1, 'clearing the background did not reach the backend')
  assert(clearedBackground.stored.enabled === false && clearedBackground.stored.path === '', `background settings were not reset: ${JSON.stringify(clearedBackground)}`)
  await page.locator('.settings-modal button[aria-label="关闭"]').click()
  await page.waitForTimeout(180)

  await page.locator('.titlebar button[aria-label="添加服务器"]').click()
  const sshImportModal = page.locator('.connection-profile-modal')
  await sshImportModal.waitFor({ state: 'visible' })
  const sshImportText = [
    '名称：Imported staging SSH',
    'ssh -p 2202 deploy@203.0.113.10',
    '密码：P@ss:w0rd!',
    '分组：Staging',
  ].join('\n')
  await pasteConnectionImport(sshImportModal.locator('.connection-import-input'), sshImportText)
  await page.waitForTimeout(120)
  assert(await sshImportModal.getByLabel('主机').inputValue() === '203.0.113.10', 'SSH formatted import did not detect the host')
  assert(await sshImportModal.getByLabel('用户').inputValue() === 'deploy', 'SSH formatted import did not detect the account')
  assert(await sshImportModal.getByLabel('端口').inputValue() === '2202', 'SSH formatted import did not detect the port')
  assert(await sshImportModal.locator('input[type="password"]').inputValue() === 'P@ss:w0rd!', 'SSH formatted import did not preserve the password')
  assert(await sshImportModal.getByLabel('分组').inputValue() === 'Staging', 'SSH formatted import did not detect the group')
  assert(!(await sshImportModal.locator('.connection-import-status').textContent())?.includes('P@ss:w0rd!'), 'SSH import status exposed the password')
  const sshAuthSelect = sshImportModal.locator('select').last()
  await sshAuthSelect.selectOption('Key')
  const privateKeyPicker = sshImportModal.locator('.private-key-input-row')
  assert(await privateKeyPicker.count() === 1, 'SSH key authentication did not show the private key picker')
  await privateKeyPicker.locator('button').click()
  await page.waitForFunction(() => (
    document.querySelector('.connection-profile-modal .private-key-input-row input')?.value ?? ''
  ).endsWith('id_ed25519'))
  assert((await privateKeyPicker.locator('input').inputValue()).endsWith('id_ed25519'), 'private key picker did not fill the selected path')
  await sshAuthSelect.selectOption('Agent')
  assert(await sshImportModal.locator('.password-field').count() === 0, 'SSH Agent authentication still required a password')
  await page.screenshot({ path: 'output/playwright/connection-import-ssh.png', fullPage: false })
  await sshImportModal.locator('button[aria-label="关闭"]').click()

  await page.locator('.dock-button[aria-label="服务器"]').click()
  await page.waitForTimeout(120)
  await page.getByRole('button', { name: '添加远程桌面' }).first().click()
  const rdpImportModal = page.locator('.remote-desktop-profile-modal')
  await rdpImportModal.waitFor({ state: 'visible' })
  const rdpImportText = [
    'name: Imported operations RDP',
    'full address:s:198.51.100.25:3390',
    'username:s:OPS\\Administrator',
    'domain:s:OPS',
    'cmdkey /generic:TERMSRV/198.51.100.25 /user:OPS\\Administrator /pass:"Rdp Secret!"',
  ].join('\n')
  await pasteConnectionImport(rdpImportModal.locator('.connection-import-input'), rdpImportText)
  await page.waitForTimeout(120)
  assert(await rdpImportModal.getByLabel('主机').inputValue() === '198.51.100.25', 'RDP formatted import did not detect the host')
  assert(await rdpImportModal.getByLabel('用户名').inputValue() === 'Administrator', 'RDP formatted import did not split domain and username')
  assert(await rdpImportModal.getByLabel('域').inputValue() === 'OPS', 'RDP formatted import did not detect the domain')
  assert(await rdpImportModal.getByLabel('端口').inputValue() === '3390', 'RDP formatted import did not detect the port')
  assert(await rdpImportModal.locator('input[type="password"]').inputValue() === 'Rdp Secret!', 'RDP formatted import did not preserve a quoted password')
  assert(!(await rdpImportModal.locator('.connection-import-status').textContent())?.includes('Rdp Secret!'), 'RDP import status exposed the password')
  await page.screenshot({ path: 'output/playwright/connection-import-rdp.png', fullPage: false })
  await rdpImportModal.locator('button[aria-label="关闭"]').click()
  await page.locator('.dock-button[aria-label="服务器"]').click()
  await page.waitForTimeout(120)

  await page.locator('.dock-button[aria-label="运行命令"]').click()
  await page.waitForTimeout(120)
  const utilityDrawer = page.locator('.workspace-drawer')
  assert(await utilityDrawer.locator('.drawer-header strong').textContent() === '运行命令', 'Run drawer title is unclear')
  assert(await utilityDrawer.locator('.inspector-tabs').count() === 0, 'activity drawer still duplicates the four navigation tabs')
  assert(await utilityDrawer.locator('.connect-card').count() === 0, 'activity drawer still mixes connection management into simple tools')
  const utilityCommandInput = utilityDrawer.locator('.utility-command-input')
  await utilityCommandInput.fill('echo sidebar-simple')
  await utilityCommandInput.press('Control+Enter')
  await page.waitForFunction(() => (
    document.querySelector('.workspace-layer.active .remote-session-local-terminal .xterm-rows')?.textContent ?? ''
  ).includes('sandbox: echo sidebar-simple'))
  await page.locator('.dock-button[aria-label="本地"]').click()
  await page.waitForTimeout(100)
  await utilityDrawer.getByRole('button', { name: '复制输出' }).click()
  const copiedTerminalOutput = await page.evaluate(() => navigator.clipboard.readText())
  assert(copiedTerminalOutput.includes('sandbox: echo sidebar-simple'), 'Copy output copied an empty or stale terminal snapshot')
  await page.locator('.dock-button[aria-label="运行命令"]').click()
  await page.waitForTimeout(100)
  await utilityDrawer.locator('.utility-command-list .command-chip').first().click()
  assert(await utilityCommandInput.inputValue() !== 'echo sidebar-simple', 'quick command did not fill the editor')
  await page.screenshot({ path: 'output/playwright/activity-run-simple.png', fullPage: false })

  await page.locator('.dock-button[aria-label="常用命令"]').click()
  await page.waitForTimeout(100)
  await utilityDrawer.getByRole('button', { name: '新建' }).click()
  assert(await utilityDrawer.locator('.utility-editor').count() === 1, 'saved-command editor did not open on demand')
  await page.screenshot({ path: 'output/playwright/activity-saved-simple.png', fullPage: false })
  await utilityDrawer.getByRole('button', { name: '取消' }).click()
  const firstSavedCommand = await utilityDrawer.locator('.snippet-item > button span').first().textContent()
  await utilityDrawer.locator('.snippet-item > button').first().click()
  await page.waitForTimeout(100)
  assert(await utilityDrawer.locator('.drawer-header strong').textContent() === '运行命令', 'saved command did not return to the Run page')
  assert(await utilityCommandInput.inputValue() === firstSavedCommand, 'saved command executed instead of filling the editor')

  await page.locator('.dock-button[aria-label="执行记录"]').click()
  await page.waitForTimeout(100)
  assert(await utilityDrawer.locator('.history-item').count() === 1, 'quick fill unexpectedly created a command history entry')
  await page.screenshot({ path: 'output/playwright/activity-history-simple.png', fullPage: false })
  await utilityDrawer.locator('.history-item').first().click()
  await page.waitForTimeout(100)
  assert(await utilityCommandInput.inputValue() === 'echo sidebar-simple', 'history item did not return the command for editing')

  const localCliLaunchers = page.locator('.workspace-layer.active .remote-session-local-terminal .terminal-cli-launchers').first()
  await localCliLaunchers.waitFor({ state: 'visible' })
  const localCliLabels = await localCliLaunchers.locator('button').evaluateAll((buttons) => buttons.map((button) => button.getAttribute('aria-label')))
  assert(localCliLabels.some((label) => label?.includes('Claude Code')), 'local terminal did not detect Claude Code')
  assert(localCliLabels.some((label) => label?.includes('Codex')), 'local terminal did not detect Codex')
  const localCliLogoState = await localCliLaunchers.locator('button').evaluateAll((buttons) => buttons.map((button) => ({
    images: button.querySelectorAll('img').length,
    text: button.textContent?.trim() ?? '',
  })))
  assert(localCliLogoState.every((item) => item.images === 1 && item.text === ''), `CLI launchers still contain letter placeholders: ${JSON.stringify(localCliLogoState)}`)
  await localCliLaunchers.locator('button[data-tool="codex"]').click()
  await page.waitForTimeout(300)
  const localCliDispatch = await page.evaluate(() => ({
    writes: window.__XUNDU_SANDBOX_LOCAL_WRITES__ ?? [],
    toast: document.querySelector('.app-toast')?.textContent ?? '',
  }))
  assert(localCliDispatch.writes.some((write) => write.data === 'codex\r'), `Codex launcher did not reach the local controller: ${JSON.stringify(localCliDispatch)}`)
  const localCliOutput = await page.locator('.workspace-layer.active .remote-session-local-terminal .xterm-rows').first().textContent()
  assert(localCliOutput?.includes('sandbox: codex'), `Codex launcher did not render in its local terminal: ${localCliOutput}`)
  const localTerminalMore = page.locator('.workspace-layer.active .remote-session-local-terminal .remote-session-toolbar button[title="更多操作"]').first()
  await localTerminalMore.click()
  await page.locator('.context-menu').waitFor({ state: 'visible' })
  assert(await utilityDrawer.locator('.drawer-header strong').textContent() === '运行命令', 'toolbar action collapsed the open drawer before its click completed')
  await page.locator('.context-menu button').first().click()

  await page.locator('.dock-button[aria-label="待办笔记"]').click()
  await page.waitForTimeout(100)
  const noteInput = utilityDrawer.locator('.utility-command-field textarea')
  await noteInput.fill('检查新版侧栏')
  await noteInput.press('Control+Enter')
  await page.waitForTimeout(100)
  const noteItem = utilityDrawer.locator('.note-item').filter({ hasText: '检查新版侧栏' })
  assert(await noteItem.count() === 1, 'simple note was not added')
  await noteItem.locator('.note-toggle').click()
  assert(await noteItem.evaluate((element) => element.classList.contains('done')), 'note completion toggle did not update')
  await page.screenshot({ path: 'output/playwright/activity-notes-simple.png', fullPage: false })
  await noteItem.locator('button[aria-label="删除笔记"]').click()
  await utilityDrawer.locator('.drawer-header button').click()
  await page.waitForTimeout(120)
  await page.screenshot({ path: 'output/playwright/theme-light-workbench.png', fullPage: false })

  await page.locator('.workbench-actions button').nth(2).click()
  await page.waitForTimeout(500)
  const filePanel = page.locator('.workspace-layer.active .file-widget').first()
  const fileAddress = filePanel.locator('.file-address-input')
  assert(await fileAddress.getAttribute('placeholder') === '此电脑', 'local file manager did not start at This PC')
  assert(await filePanel.locator('.file-row.virtual').filter({ hasText: 'C:\\' }).count() >= 1, 'This PC is missing the C drive')
  assert(await filePanel.locator('.file-row.virtual').filter({ hasText: 'F:\\' }).count() >= 1, 'This PC is missing the F drive')

  await fileAddress.fill('"F:/Sandbox"')
  await fileAddress.press('Enter')
  await page.waitForTimeout(250)
  assert(await fileAddress.inputValue() === 'F:/Sandbox', `quoted address was not normalized: ${await fileAddress.inputValue()}`)
  assert(await filePanel.locator('.file-row.virtual').filter({ hasText: 'output' }).count() >= 1, 'Enter did not navigate to the typed folder path')

  await filePanel.locator('.file-toolbar button[aria-label="此电脑"]').click()
  await page.waitForTimeout(200)
  assert(await fileAddress.getAttribute('placeholder') === '此电脑', 'This PC button did not return to the drive list')
  await fileAddress.fill('C:/Users/sandbox')
  await fileAddress.press('Enter')
  await page.waitForTimeout(250)
  assert(await fileAddress.inputValue() === 'C:/Users/sandbox', 'file address did not render the submitted local path')

  await page.locator('.workspace-layer.active .file-row.virtual').filter({ hasText: 'results.json' }).first().dblclick()
  await page.waitForTimeout(300)
  const editorTheme = await page.evaluate(() => {
    const modal = document.querySelector('.file-editor-modal')
    const editor = document.querySelector('.file-code-editor .cm-editor')
    const gutters = document.querySelector('.file-code-editor .cm-gutters')
    return {
      modal: modal ? getComputedStyle(modal).backgroundColor : '',
      editor: editor ? getComputedStyle(editor).backgroundColor : '',
      foreground: editor ? getComputedStyle(editor).color : '',
      gutters: gutters ? getComputedStyle(gutters).backgroundColor : '',
    }
  })
  assert(editorTheme.modal === 'rgb(251, 251, 252)', `light file editor modal kept a dark background: ${editorTheme.modal}`)
  assert(editorTheme.editor === 'rgb(251, 251, 252)', `light CodeMirror surface kept a dark background: ${editorTheme.editor}`)
  assert(editorTheme.foreground === 'rgb(37, 40, 46)', `light CodeMirror text color is incorrect: ${editorTheme.foreground}`)
  assert(editorTheme.gutters !== 'rgb(5, 6, 8)', `light CodeMirror gutters kept the old hardcoded color: ${editorTheme.gutters}`)
  await page.screenshot({ path: 'output/playwright/theme-light-editor.png', fullPage: false })
  await page.locator('.file-editor-close').click()
  await page.waitForTimeout(150)

  const resultsFile = filePanel.locator('.file-row.virtual').filter({ hasText: 'results.json' }).first()
  await resultsFile.dblclick()
  await page.waitForTimeout(300)
  const freshReadState = await page.evaluate(() => ({
    reads: Math.max(0, ...Object.values(window.__XUNDU_SANDBOX_FILE_READS__ ?? {})),
    content: document.querySelector('.file-code-editor .cm-content')?.textContent ?? '',
  }))
  assert(freshReadState.reads === 2, `reopening a file reused cached content: reads=${freshReadState.reads}`)
  assert(freshReadState.content.includes('"revision": 2'), 'reopened editor did not render the latest server content')
  await page.locator('.file-editor-close').click()
  await page.waitForTimeout(150)

  await resultsFile.click({ button: 'right' })
  const fileMenuLabels = await page.locator('.context-menu button span').allTextContents()
  assert(fileMenuLabels.includes('另存为'), 'local file context menu is missing Save As')
  assert(fileMenuLabels.includes('重命名'), 'file context menu is missing rename')
  assert(fileMenuLabels.includes('删除文件'), 'file context menu is missing delete')
  await page.locator('.context-menu').getByRole('button', { name: /另存为/ }).click()
  await page.locator('.file-download-progress').waitFor({ state: 'visible' })
  await page.locator('.file-download-progress.status-completed').waitFor({ state: 'visible', timeout: 4_000 })
  const completedDownload = await page.evaluate(() => (window.__XUNDU_SANDBOX_FILE_DOWNLOADS__ ?? []).at(-1))
  assert(String(completedDownload).endsWith('results.json'), `file download did not use the selected path: ${completedDownload}`)
  await page.screenshot({ path: 'output/playwright/file-download-completed.png', fullPage: false })
  await page.locator('.file-download-heading button').click()

  await resultsFile.click({ button: 'right' })
  await page.locator('.context-menu').getByRole('button', { name: /另存为/ }).click()
  await page.locator('.file-download-progress.status-running').waitFor({ state: 'visible' })
  await page.locator('.file-download-heading button').click()
  await page.locator('.file-download-progress.status-cancelled').waitFor({ state: 'visible', timeout: 4_000 })
  await page.locator('.file-download-heading button').click()

  const storedTransfers = await page.evaluate(() => JSON.parse(localStorage.getItem('xundu.phase2.transfers.v1') ?? '[]'))
  assert(storedTransfers.some((transfer) => transfer.status === 'completed'), 'completed file download did not enter the unified transfer history')
  assert(storedTransfers.some((transfer) => transfer.status === 'cancelled'), 'cancelled file download did not enter the unified transfer history')
  assert(!JSON.stringify(storedTransfers).includes('"password"'), 'transfer history persisted a credential field')
  await page.getByRole('button', { name: '文件传输管理' }).click()
  const initialTransferManager = page.getByRole('dialog', { name: '文件传输管理' })
  await initialTransferManager.waitFor({ state: 'visible' })
  assert(await initialTransferManager.locator('.transfer-manager-item').count() >= 2, 'transfer manager did not show completed and cancelled downloads')
  assert(await initialTransferManager.locator('.transfer-manager-item.status-completed').count() >= 1, 'transfer manager is missing the completed download')
  assert(await initialTransferManager.locator('.transfer-manager-item.status-cancelled').count() >= 1, 'transfer manager is missing the cancelled download')
  await initialTransferManager.getByRole('button', { name: '关闭' }).click()
  await initialTransferManager.waitFor({ state: 'hidden' })
  assert(await page.locator('.dock-button[aria-label="任务中心"]').count() === 0, 'removed task center is still visible in the navigation rail')

  const globalSearch = page.getByLabel('搜索 SSH 服务器和远程桌面')
  const drawerStateBeforeSearch = await page.locator('.app-shell').getAttribute('class')
  await page.evaluate(() => { window.__XUNDU_SANDBOX_APP_RENDERS__ = 0 })
  await globalSearch.pressSequentially('192.0.2.201', { delay: 4 })
  await page.locator('.global-search-popover').waitFor({ state: 'visible' })
  assert(await page.locator('.global-search-result').count() === 1, 'global search did not narrow the connection results')
  assert((await page.locator('.global-search-result').textContent())?.includes('192.0.2.201'), 'global search returned the wrong server')
  assert(await page.locator('.app-shell').getAttribute('class') === drawerStateBeforeSearch, 'typing in global search changed the workspace drawer layout')
  const appRendersDuringSearch = await page.evaluate(() => window.__XUNDU_SANDBOX_APP_RENDERS__ ?? 0)
  assert(appRendersDuringSearch === 0, `typing in global search rerendered the app ${appRendersDuringSearch} times`)
  await globalSearch.fill('__missing-connection__')
  await page.locator('.global-search-empty').waitFor({ state: 'visible' })
  await globalSearch.press('Escape')
  await page.locator('.global-search-popover').waitFor({ state: 'hidden' })
  await globalSearch.fill('38.55')
  await page.getByRole('button', { name: '清除搜索' }).click()
  assert(await globalSearch.inputValue() === '', 'global search clear button did not reset the query')
  await globalSearch.fill('192.0.2.201')
  await page.locator('.global-search-result').waitFor({ state: 'visible' })
  const sshConnectsBeforeSearch = await page.evaluate(() => window.__XUNDU_SANDBOX_SSH_CONNECTS__ ?? 0)
  await globalSearch.press('Enter')
  await page.locator('.global-search-popover').waitFor({ state: 'hidden' })
  assert(await globalSearch.inputValue() === '', 'activating a global search result did not reset the query')
  await page.waitForFunction(
    (before) => (window.__XUNDU_SANDBOX_SSH_CONNECTS__ ?? 0) === before + 1,
    sshConnectsBeforeSearch,
  )

  const localPanelStarted = Date.now()
  if (await page.locator('.app-shell.drawer-open').count()) {
    await page.getByRole('button', { name: '收起侧边栏' }).click()
    await page.locator('.app-shell.drawer-collapsed').waitFor({ state: 'attached' })
  }
  const workbenchBeforeDrawer = await page.locator('.wave-workbench').boundingBox()
  await page.locator('.dock-button[aria-label="本地"]').click()
  await page.locator('.local-tools').waitFor({ state: 'visible' })
  assert(Date.now() - localPanelStarted < 1_000, 'local sidebar took too long to become interactive')
  const drawerMotion = await page.locator('.workspace-drawer').evaluate((drawer) => {
    const style = getComputedStyle(drawer)
    return {
      properties: style.transitionProperty,
      duration: style.transitionDuration,
      willChange: style.willChange,
    }
  })
  const workbenchAfterDrawer = await page.locator('.wave-workbench').boundingBox()
  assert(drawerMotion.properties.includes('transform') && drawerMotion.properties.includes('opacity'), `sidebar is missing compositor transitions: ${JSON.stringify(drawerMotion)}`)
  assert(drawerMotion.willChange.includes('transform'), `sidebar is not promoted for smooth motion: ${JSON.stringify(drawerMotion)}`)
  assert(
    Math.abs(workbenchBeforeDrawer.width - workbenchAfterDrawer.width) < 1
      && Math.abs(workbenchBeforeDrawer.x - workbenchAfterDrawer.x) < 1,
    `opening the overlay sidebar resized the workbench: ${JSON.stringify({ workbenchBeforeDrawer, workbenchAfterDrawer })}`,
  )
  await page.getByRole('button', { name: '收起侧边栏' }).click()
  await page.locator('.app-shell.drawer-collapsed').waitFor({ state: 'attached' })

  const paletteStarted = Date.now()
  await page.getByRole('button', { name: '命令面板' }).click()
  const paletteSearch = page.getByLabel('搜索命令、片段、设置...')
  await paletteSearch.waitFor({ state: 'visible' })
  assert(Date.now() - paletteStarted < 1_000, 'command palette took too long to open')
  await paletteSearch.fill('设置')
  await page.getByRole('button', { name: /打开设置/ }).waitFor({ state: 'visible' })
  assert(await page.locator('.palette-item').count() === 1, 'command palette search did not narrow the results')
  await page.getByRole('button', { name: '关闭命令面板' }).click()

  await resultsFile.click({ button: 'right' })
  await page.getByRole('button', { name: /重命名/ }).click()
  const renameModal = page.locator('.file-operation-modal')
  await renameModal.waitFor({ state: 'visible' })
  await renameModal.locator('input').fill('results-renamed.json')
  await renameModal.getByRole('button', { name: '取消' }).click()
  await resultsFile.click({ button: 'right' })
  await page.getByRole('button', { name: /删除文件/ }).click()
  const deleteModal = page.locator('.file-operation-modal')
  await deleteModal.waitFor({ state: 'visible' })
  assert(await deleteModal.getByRole('button', { name: '确认删除' }).count() === 1, 'delete confirmation action is missing')
  await deleteModal.getByRole('button', { name: '取消' }).click()

  await page.locator('.appearance-switch button').last().click()
  const darkTerminalToken = await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--terminal-bg').trim())
  assert(darkTerminalToken === '#111216', `dark terminal token did not follow appearance: ${darkTerminalToken}`)

  const localTerminal = page.locator('.remote-session-local-terminal').first()
  const localTerminalScreen = localTerminal.locator('.xterm-screen')
  await localTerminalScreen.click()
  await localTerminalScreen.click({ button: 'right', position: { x: 24, y: 24 } })
  const terminalContextMenu = page.locator('.context-menu')
  await terminalContextMenu.waitFor({ state: 'visible' })
  assert(await terminalContextMenu.locator('button').count() === 2, 'terminal right-click menu did not expose copy and paste')
  assert(await terminalContextMenu.locator('button').first().isDisabled(), 'terminal copy action should be disabled without a selection')
  assert(await terminalContextMenu.locator('button').nth(1).isEnabled(), 'terminal paste action should stay available without a selection')
  await localTerminalScreen.click()

  const terminalScreenBox = await localTerminalScreen.boundingBox()
  assert(Boolean(terminalScreenBox), 'could not measure terminal screen for selection copy test')
  await page.evaluate(() => navigator.clipboard.writeText(''))
  const interruptsBeforeSelectionCopy = await page.evaluate(() => (
    window.__XUNDU_SANDBOX_LOCAL_WRITES__ ?? []
  ).filter((write) => write.data.includes('\x03')).length)
  await page.mouse.move(terminalScreenBox.x + 12, terminalScreenBox.y + 10)
  await page.mouse.down()
  await page.mouse.move(terminalScreenBox.x + 190, terminalScreenBox.y + 10, { steps: 8 })
  await page.mouse.up()
  await page.keyboard.press('Control+C')
  await page.waitForTimeout(120)
  const selectionCopyState = await page.evaluate(() => ({
    text: '',
    interrupts: (window.__XUNDU_SANDBOX_LOCAL_WRITES__ ?? []).filter((write) => write.data.includes('\x03')).length,
  }))
  selectionCopyState.text = await page.evaluate(() => navigator.clipboard.readText())
  assert(selectionCopyState.text.trim().length > 0, 'Ctrl+C did not copy the selected terminal text')
  assert(selectionCopyState.interrupts === interruptsBeforeSelectionCopy, 'Ctrl+C sent an interrupt while terminal text was selected')

  const wrapProbe = await localTerminal.evaluate((panel) => {
    const screen = panel.querySelector('.xterm-screen')
    const textarea = panel.querySelector('.xterm-helper-textarea')
    const measure = panel.querySelector('.xterm-char-measure-element')
    if (!(screen instanceof HTMLElement) || !(textarea instanceof HTMLElement) || !(measure instanceof HTMLElement)) return null
    const screenBox = screen.getBoundingClientRect()
    const measureBox = measure.getBoundingClientRect()
    const measuredCharacters = measure.textContent?.length ?? 0
    const characterWidth = measuredCharacters ? measureBox.width / measuredCharacters : 0
    if (!characterWidth) return null
    const cols = Math.max(1, Math.round(screenBox.width / characterWidth))
    const cursorX = Math.max(0, Math.round(Number.parseFloat(getComputedStyle(textarea).left) / characterWidth))
    return { charsToWrap: Math.max(1, cols - cursorX) }
  })
  assert(Boolean(wrapProbe), 'could not measure local terminal cursor for wrapped backspace test')
  const wrappedInput = 'q'.repeat(wrapProbe.charsToWrap)
  await localTerminal.locator('.xterm-helper-textarea').focus()
  await page.keyboard.type(wrappedInput)
  await page.waitForTimeout(200)
  const wrappedCountBeforeBackspace = countCharacter(await localTerminal.locator('.xterm-rows').textContent(), 'q')
  await page.keyboard.press('Backspace')
  await page.waitForTimeout(120)
  const wrappedCountAfterBackspace = countCharacter(await localTerminal.locator('.xterm-rows').textContent(), 'q')
  assert(
    wrappedCountBeforeBackspace > 0 && wrappedCountAfterBackspace < wrappedCountBeforeBackspace,
    `wrapped backspace did not erase previous row: ${wrappedCountBeforeBackspace} -> ${wrappedCountAfterBackspace}`,
  )

  await page.locator('.dock-button').nth(0).click()
  await page.waitForTimeout(250)
  const sshServerItems = page.locator('.server-source-list .source-section').first().locator('.server-item')
  await expectCount(page, '.server-source-list .source-section:first-child .server-item', 4, 'sandbox server list')
  assert(await page.evaluate(() => (window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0) === 0), 'RDP connected during application startup')
  assert(await page.getByRole('checkbox', { name: '文件管理' }).isChecked(), 'file manager should open by default')
  assert(!await page.getByRole('checkbox', { name: '机器监控' }).isChecked(), 'monitor should stay opt-in')

  await sshServerItems.first().click({ button: 'right' })
  await page.waitForTimeout(150)
  await page.locator('.context-menu button').nth(1).click()
  await page.waitForTimeout(700)
  await ensureServerDrawer(page)
  await sshServerItems.first().click({ button: 'right' })
  await page.waitForTimeout(150)
  await page.locator('.context-menu button').nth(2).click()
  await page.waitForTimeout(900)

  await ensureServerDrawer(page)
  const sshConnectCountBeforeDoubleClick = await page.evaluate(() => window.__XUNDU_SANDBOX_SSH_CONNECTS__ ?? 0)
  await sshServerItems.first().dblclick()
  await page.waitForTimeout(1800)
  const firstSshPanel = page.locator('.workspace-layer.active .remote-session-ssh-terminal').filter({ hasText: '192.0.2.198' }).last()
  const sshConnectCount = await page.evaluate(() => window.__XUNDU_SANDBOX_SSH_CONNECTS__ ?? 0)
  assert(sshConnectCount === sshConnectCountBeforeDoubleClick + 1, `double-click SSH connect did not invoke exactly once: before=${sshConnectCountBeforeDoubleClick} after=${sshConnectCount}`)
  assert(await firstSshPanel.locator('.terminal-standby-host').count() === 0, 'double-click left the SSH terminal in standby')
  assert((await firstSshPanel.textContent())?.includes('实时接收中'), 'mock SSH terminal did not enter the live receiving state')
  const firstSshTextarea = firstSshPanel.locator('.xterm-helper-textarea')
  await firstSshTextarea.focus()
  await page.evaluate(() => { window.__XUNDU_SANDBOX_INPUT_STARTED_AT__ = performance.now() })
  await page.keyboard.insertText('Q')
  const firstSshPanelHandle = await firstSshPanel.elementHandle()
  await page.waitForFunction((panel) => [...panel.querySelectorAll('.xterm-rows > div')]
    .some((row) => row.textContent?.trimEnd().endsWith('# Q')), firstSshPanelHandle)
  const singleCharacterEchoMs = await page.evaluate(() => performance.now() - window.__XUNDU_SANDBOX_INPUT_STARTED_AT__)
  assert(singleCharacterEchoMs < 120, `single-character SSH echo path was too slow: ${singleCharacterEchoMs.toFixed(1)}ms`)
  await page.keyboard.press('Backspace')
  const remoteCliLaunchers = firstSshPanel.locator('.terminal-cli-launchers')
  await remoteCliLaunchers.waitFor({ state: 'visible' })
  assert(await remoteCliLaunchers.locator('button[data-tool="claude"]').count() === 1, 'SSH terminal did not detect remote Claude Code')
  await remoteCliLaunchers.locator('button[data-tool="claude"]').click()
  await page.waitForFunction(() => (window.__XUNDU_SANDBOX_SSH_WRITES__ ?? []).some((write) => write.data === 'claude\r'))
  await page.waitForFunction(() => [...document.querySelectorAll('.workspace-layer.active .remote-session-ssh-terminal')]
    .some((panel) => panel.textContent?.includes('192.0.2.198') && panel.textContent.includes('sandbox output: claude')))

  await firstSshPanel.locator('.xterm-helper-textarea').focus()
  await page.keyboard.type('bt')
  await page.keyboard.press('Enter')
  await page.waitForFunction(() => [...document.querySelectorAll('.workspace-layer.active .remote-session-ssh-terminal')]
    .some((panel) => panel.textContent?.includes('192.0.2.198') && panel.textContent.includes('请输入命令编号：')))
  const btPromptGeometry = await firstSshPanel.evaluate((panel) => {
    const host = panel.querySelector('.widget-terminal-host')?.getBoundingClientRect()
    const promptRow = [...panel.querySelectorAll('.xterm-rows > div')]
      .findLast((row) => row.textContent?.includes('请输入命令编号：'))
      ?.getBoundingClientRect()
    return host && promptRow
      ? {
          visible: promptRow.top >= host.top && promptRow.bottom <= host.bottom,
          hostTop: host.top,
          hostBottom: host.bottom,
          promptTop: promptRow.top,
          promptBottom: promptRow.bottom,
        }
      : null
  })
  assert(btPromptGeometry?.visible, `interactive SSH prompt was clipped below the terminal: ${JSON.stringify(btPromptGeometry)}`)

  await ensureServerDrawer(page)
  await sshServerItems.nth(1).click()
  await page.waitForTimeout(250)
  const firstServerDotClass = await sshServerItems.first().locator('.connection-dot').getAttribute('class')
  assert(
    firstServerDotClass?.includes('connected'),
    `connected server lost its list state after selecting another server: ${firstServerDotClass}`,
  )

  for (const serverIndex of [1, 2]) {
    await ensureServerDrawer(page)
    await sshServerItems.nth(serverIndex).dblclick()
    await page.waitForTimeout(900)
  }

  if (await sshServerItems.count()) {
    await page.locator('.dock-button').nth(0).click()
  }
  await page.waitForTimeout(250)
  const remoteTerminalCount = await page.locator('.workspace-layer.active .remote-session-ssh-terminal').count()
  const liveRemoteTerminalCount = await page.locator('.workspace-layer.active .remote-session-ssh-terminal .remote-xterm-host').count()
  const sleepingRemoteTerminalCount = await page.locator('.workspace-layer.active .remote-session-ssh-terminal .terminal-sleep-host').count()
  assert(remoteTerminalCount >= 3, `expected at least three remote terminals, got ${remoteTerminalCount}`)
  assert(liveRemoteTerminalCount === remoteTerminalCount, `${remoteTerminalCount - liveRemoteTerminalCount} active terminals were downgraded to previews`)
  assert(sleepingRemoteTerminalCount === 0, `${sleepingRemoteTerminalCount} active terminals still use static previews`)

  const secondRemoteTerminal = page.locator('.workspace-layer.active .remote-session-ssh-terminal').nth(1)
  const secondRemoteScreen = secondRemoteTerminal.locator('.xterm-screen')
  await secondRemoteScreen.click()
  await secondRemoteScreen.click({ button: 'right', position: { x: 30, y: 30 } })
  const secondTerminalMenu = page.locator('body > .context-menu')
  await secondTerminalMenu.waitFor({ state: 'visible' })
  assert(await secondTerminalMenu.locator('button').count() === 2, 'second terminal right-click menu was clipped or missing')
  await secondTerminalMenu.locator('button:not(:disabled)').first().click()
  await secondTerminalMenu.waitFor({ state: 'hidden' })
  await secondRemoteScreen.click()

  const secondRemoteScreenBox = await secondRemoteScreen.boundingBox()
  assert(Boolean(secondRemoteScreenBox), 'could not measure the second SSH terminal for selection copy')
  await page.evaluate(() => navigator.clipboard.writeText(''))
  const secondTerminalInterruptsBefore = await page.evaluate(() => (
    window.__XUNDU_SANDBOX_SSH_WRITES__ ?? []
  ).filter((write) => write.data.includes('\x03')).length)
  await page.mouse.move(secondRemoteScreenBox.x + 12, secondRemoteScreenBox.y + 10)
  await page.mouse.down()
  await page.mouse.move(
    Math.min(secondRemoteScreenBox.x + 190, secondRemoteScreenBox.x + secondRemoteScreenBox.width - 12),
    secondRemoteScreenBox.y + 10,
    { steps: 8 },
  )
  await page.mouse.up()
  await page.keyboard.press('Control+C')
  await page.waitForTimeout(120)
  const secondTerminalCopyState = await page.evaluate(async () => ({
    text: await navigator.clipboard.readText(),
    interrupts: (window.__XUNDU_SANDBOX_SSH_WRITES__ ?? []).filter((write) => write.data.includes('\x03')).length,
  }))
  assert(secondTerminalCopyState.text.trim().length > 0, 'Ctrl+C did not copy text from the second SSH terminal')
  assert(secondTerminalCopyState.interrupts === secondTerminalInterruptsBefore, 'second terminal sent Ctrl+C to SSH while text was selected')

  const compositionTerminal = page.locator('.workspace-layer.active .remote-session-ssh-terminal').last()
  await compositionTerminal.locator('.xterm-helper-textarea').focus()
  await page.keyboard.insertText('中文输入')
  await page.waitForTimeout(220)
  const compositionText = await compositionTerminal.locator('.xterm-rows').textContent()
  assert(compositionText?.includes('中文输入'), 'committed Chinese IME text did not reach the live terminal')
  await page.keyboard.press('Enter')
  await page.waitForTimeout(120)

  const compositionTextarea = compositionTerminal.locator('.xterm-helper-textarea')
  await compositionTextarea.evaluate((element) => {
    const longPreedit = `echo ${'wrapped-command-'.repeat(18)}`
    element.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }))
    element.dispatchEvent(new CompositionEvent('compositionupdate', { bubbles: true, data: longPreedit }))
  })
  await page.waitForTimeout(80)
  const compositionWrap = await compositionTerminal.evaluate((element) => {
    const mirror = element.querySelector('.terminal-composition-mirror.active')
    const screen = element.querySelector('.xterm-screen')
    const style = mirror ? getComputedStyle(mirror) : null
    const lineHeight = Number.parseFloat(style?.lineHeight ?? '0')
    const mirrorBox = mirror?.getBoundingClientRect()
    const screenBox = screen?.getBoundingClientRect()
    return {
      active: Boolean(mirror),
      wrapped: Boolean(mirrorBox && lineHeight > 0 && mirrorBox.height > lineHeight * 1.5),
      insideTerminal: Boolean(mirrorBox && screenBox && mirrorBox.left >= screenBox.left - 1 && mirrorBox.right <= screenBox.right + 1),
      preservesFullText: mirror?.textContent?.length ?? 0,
    }
  })
  assert(compositionWrap.active, 'long IME preedit did not use the terminal composition mirror')
  assert(compositionWrap.wrapped, 'long IME preedit stayed on a single line')
  assert(compositionWrap.insideTerminal, 'wrapped IME preedit overflowed the terminal width')
  assert(compositionWrap.preservesFullText > 200, 'wrapped IME preedit truncated command text')
  await compositionTextarea.evaluate((element) => {
    element.dispatchEvent(new CompositionEvent('compositionupdate', {
      bubbles: true,
      data: 'echo edited-command --flag',
    }))
  })
  await page.waitForTimeout(40)
  assert(
    await compositionTerminal.locator('.terminal-composition-mirror.active').textContent() === 'echo edited-command --flag',
    'IME preedit did not stay synchronized after deleting or editing a wrapped command',
  )
  await compositionTextarea.evaluate((element) => {
    element.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '' }))
  })

  await compositionTextarea.evaluate((element) => {
    element.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }))
    element.dispatchEvent(new CompositionEvent('compositionupdate', { bubbles: true, data: "c'd" }))
    element.dispatchEvent(new KeyboardEvent('keydown', {
      bubbles: true,
      key: 'Shift',
      code: 'ShiftLeft',
      shiftKey: true,
    }))
    element.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '' }))
    element.dispatchEvent(new KeyboardEvent('keyup', {
      bubbles: true,
      key: 'Shift',
      code: 'ShiftLeft',
    }))
  })
  await page.waitForTimeout(180)
  const shiftCommitText = await compositionTerminal.locator('.xterm-rows').textContent()
  assert(shiftCommitText?.includes('~# cd'), `Shift IME switch did not preserve the raw cd preedit: ${JSON.stringify(shiftCommitText)}`)

  const terminalTextBeforeWorkspaceSwitch = await page.locator('.workspace-layer.active .xterm-rows').last().textContent()
  await page.locator('.task-tab').nth(1).click()
  await page.locator('.task-tab').nth(0).click()
  await page.waitForTimeout(250)
  const terminalTextAfterWorkspaceSwitch = await page.locator('.workspace-layer.active .xterm-rows').last().textContent()
  assert(
    terminalTextBeforeWorkspaceSwitch?.includes('cd')
      && terminalTextAfterWorkspaceSwitch?.includes('cd')
      && terminalTextAfterWorkspaceSwitch.includes('sandbox stream'),
    'terminal content was not preserved after switching workspaces',
  )

  const persistentFilePanel = page.locator('.workspace-layer.active .file-widget').first()
  await persistentFilePanel.locator('.file-row.virtual').filter({ hasText: 'output' }).first().dblclick()
  await page.waitForTimeout(250)
  const filePathBeforeWorkspaceSwitch = await persistentFilePanel.locator('.file-address-input').inputValue()
  assert(filePathBeforeWorkspaceSwitch?.replace(/\\/g, '/').endsWith('/output'), `file manager did not enter the test directory: ${filePathBeforeWorkspaceSwitch}`)
  await page.locator('.task-tab').nth(1).click()
  await page.locator('.task-tab').nth(0).click()
  await page.waitForTimeout(1_200)
  const filePathAfterWorkspaceSwitch = await persistentFilePanel.locator('.file-address-input').inputValue()
  assert(
    filePathAfterWorkspaceSwitch === filePathBeforeWorkspaceSwitch,
    `file manager path reset after switching workspaces: ${filePathBeforeWorkspaceSwitch} -> ${filePathAfterWorkspaceSwitch}`,
  )
  const fileToolbarGeometry = await page.locator('.workspace-layer.active .remote-session-files .remote-session-toolbar').evaluateAll((toolbars) => toolbars.map((toolbar) => {
    const actions = toolbar.querySelector('.remote-session-actions')
    const toolbarRect = toolbar.getBoundingClientRect()
    const actionsRect = actions?.getBoundingClientRect()
    return {
      rightGap: actionsRect ? toolbarRect.right - actionsRect.right : null,
      actionsWidth: actionsRect?.width ?? 0,
    }
  }))
  assert(fileToolbarGeometry.length > 0, 'file toolbar geometry regression had no panel to inspect')
  assert(
    fileToolbarGeometry.every(({ rightGap, actionsWidth }) => rightGap !== null && rightGap >= 0 && rightGap <= 10 && actionsWidth > 0),
    `file toolbar actions left an empty reserved column: ${JSON.stringify(fileToolbarGeometry)}`,
  )

  const workspaceCountBeforeCloseCheck = await page.locator('.workbench-tabs .task-tab').count()
  await page.locator('.workbench-tabs .task-add').click()
  await page.waitForTimeout(80)
  assert(
    await page.locator('.workbench-tabs .task-tab').count() === workspaceCountBeforeCloseCheck + 1,
    'workspace tab was not added for the close lifecycle check',
  )
  const addedWorkspaceShell = page.locator('.workbench-tabs .task-tab-shell').last()
  assert(await addedWorkspaceShell.locator('.task-tab').getAttribute('aria-selected') === 'true', 'new workspace did not become active')
  await addedWorkspaceShell.locator('.task-tab-close').click()
  await page.waitForTimeout(100)
  assert(
    await page.locator('.workbench-tabs .task-tab').count() === workspaceCountBeforeCloseCheck,
    'workspace close button did not remove its tab',
  )
  assert(
    await page.locator('.workbench-tabs .task-tab[aria-selected="true"]').count() === 1,
    'closing the active workspace did not select a neighboring tab',
  )
  await page.locator('.workbench-tabs .task-tab').first().click()
  await page.waitForTimeout(80)

  const refreshPanel = page.locator('.remote-session-panel.remote-session-ssh-terminal').filter({ hasText: '192.0.2.198' }).last()
  const refreshWidgetId = await refreshPanel.getAttribute('data-workbench-widget-id')
  const refreshStarted = Date.now()
  await refreshPanel.locator('button[title="刷新窗口"]').click({ timeout: 1_500 })
  assert(
    await refreshPanel.getAttribute('data-workbench-widget-id') === refreshWidgetId,
    'refresh replaced the stable widget identity',
  )
  assert(await refreshPanel.locator('button[title="刷新窗口"]').isDisabled(), 'refresh lock was lost after session restart')
  await page.locator('.task-tab').nth(1).click({ timeout: 1_500 })
  await page.locator('.task-tab').nth(0).click({ timeout: 1_500 })
  assert(Date.now() - refreshStarted < 3_000, 'refresh blocked workspace interaction')

  await ensureServerDrawer(page)
  const desktopSourceSection = page.locator('.server-source-list .remote-desktop-source-section')
  await desktopSourceSection.locator('.small-control').click()
  const desktopProfileModal = page.locator('.remote-desktop-profile-modal')
  const desktopProfileInputs = desktopProfileModal.locator('input')
  await desktopProfileInputs.nth(0).fill('Sandbox Desktop')
  await desktopProfileInputs.nth(1).fill('sandbox-rdp')
  await desktopProfileInputs.nth(2).fill('sandbox-user')
  await desktopProfileInputs.nth(4).fill('sandbox-password')
  await desktopProfileModal.locator('button[type="submit"]').click()
  await page.waitForTimeout(120)
  await page.waitForFunction(() => {
    const profiles = JSON.parse(localStorage.getItem('xundu.remoteDesktopProfiles') ?? '[]')
    const vault = JSON.parse(sessionStorage.getItem('__xundu.sandbox.credentials') ?? '{}')
    return profiles.length === 1
      && profiles.every((profile) => !Object.hasOwn(profile, 'password'))
      && Object.keys(vault).some((key) => key.startsWith('rdp-profile:'))
  })
  const savedDesktopProfile = desktopSourceSection.locator('.remote-desktop-profile-item').filter({ hasText: 'Sandbox Desktop' })
  assert(await savedDesktopProfile.count() === 1, 'remote desktop profile was not added to the server sidebar')
  await savedDesktopProfile.click()
  await page.waitForTimeout(160)
  const desktopPanel = page.locator('.workspace-layer.active .remote-session-remote-desktop').last()
  await desktopPanel.locator('.remote-desktop-shell[data-status="disconnected"]').waitFor({ timeout: 2_000 })
  await page.waitForFunction(() => {
    const workspaces = JSON.parse(localStorage.getItem('xundu.workspaces.v2') ?? '[]')
    const vault = JSON.parse(sessionStorage.getItem('__xundu.sandbox.credentials') ?? '{}')
    const desktopWidgets = workspaces.flatMap((workspace) => workspace.widgets ?? [])
      .filter((widget) => widget.type === 'remote-desktop')
    return desktopWidgets.length > 0
      && desktopWidgets.every((widget) => !Object.hasOwn(widget.remoteDesktop ?? {}, 'password'))
      && Object.keys(vault).some((key) => key.startsWith('rdp-widget:'))
  })
  assert(await desktopPanel.locator('.remote-desktop-status button').count() === 1, 'standby RDP panel did not show its central connect button')
  assert(await page.evaluate(() => (window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0) === 0), 'opening a saved RDP profile connected before user confirmation')
  await desktopPanel.locator('.remote-desktop-status button').click()
  try {
    await desktopPanel.locator('.remote-desktop-shell[data-status="connected"]').waitFor({ timeout: 3_000 })
  } catch (error) {
    throw new Error(`remote desktop did not connect: ${JSON.stringify({ text: await desktopPanel.textContent(), errors: pageErrors })}`, { cause: error })
  }
  assert(
    await page.evaluate(() => (window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0) === 2),
    'RDP startup timeout did not retry exactly once with TLS',
  )
  await page.waitForTimeout(160)
  const desktopRender = await desktopPanel.evaluate((panel) => {
    const viewport = panel.querySelector('.remote-desktop-viewport')?.getBoundingClientRect()
    const canvas = panel.querySelector('.remote-desktop-display canvas')
    const canvasBox = canvas?.getBoundingClientRect()
    let nonBlank = false
    if (canvas instanceof HTMLCanvasElement) {
      const context = canvas.getContext('2d')
      const pixel = context?.getImageData(Math.min(50, canvas.width - 1), Math.min(50, canvas.height - 1), 1, 1).data
      nonBlank = Boolean(pixel && pixel[3] > 0)
    }
    return {
      canvas: Boolean(canvas),
      nonBlank,
      contained: Boolean(viewport && canvasBox && canvasBox.left >= viewport.left - 1 && canvasBox.top >= viewport.top - 1 && canvasBox.right <= viewport.right + 1 && canvasBox.bottom <= viewport.bottom + 1),
      storedHost: JSON.parse(localStorage.getItem('xundu.workspaces.v2') || '[]')
        .flatMap((workspace) => workspace.widgets || [])
        .find((widget) => widget.type === 'remote-desktop')?.remoteDesktop?.host,
    }
  })
  assert(desktopRender.canvas, 'Native RDP remote desktop canvas was not created')
  assert(desktopRender.nonBlank, 'Native RDP remote desktop canvas stayed blank')
  assert(desktopRender.contained, 'remote desktop canvas escaped its widget viewport')
  assert(desktopRender.storedHost === 'sandbox-rdp', 'remote desktop connection was not persisted')
  assert(await desktopPanel.locator('.remote-desktop-controls button').nth(0).isEnabled(), 'remote clipboard control did not enable after connecting')
  const rdpConnectsBeforeEdit = await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0)
  await desktopPanel.locator('.remote-desktop-controls button[title="编辑连接"]').click()
  const liveEditForm = desktopPanel.locator('.remote-desktop-editor-overlay .remote-desktop-form')
  await liveEditForm.waitFor({ state: 'visible' })
  assert(await desktopPanel.locator('.remote-desktop-shell').getAttribute('data-status') === 'connected', 'opening RDP edit disconnected the live session')
  assert(await desktopPanel.locator('.remote-desktop-display canvas').count() === 1, 'opening RDP edit unmounted the live canvas')
  assert(await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0) === rdpConnectsBeforeEdit, 'opening RDP edit restarted the session')
  await page.screenshot({ path: 'output/playwright/rdp-live-edit-overlay.png', fullPage: false })
  await liveEditForm.getByRole('button', { name: '取消' }).click()
  await liveEditForm.waitFor({ state: 'hidden' })
  assert(await desktopPanel.locator('.remote-desktop-shell').getAttribute('data-status') === 'connected', 'cancelling unchanged RDP edit disconnected the session')
  assert(await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0) === rdpConnectsBeforeEdit, 'cancelling unchanged RDP edit reconnected the session')

  await desktopPanel.locator('.remote-desktop-controls button[title="编辑连接"]').click()
  await liveEditForm.getByRole('button', { name: '保存并连接' }).click()
  await liveEditForm.waitFor({ state: 'hidden' })
  assert(await desktopPanel.locator('.remote-desktop-shell').getAttribute('data-status') === 'connected', 'saving unchanged RDP settings disconnected the session')
  assert(await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0) === rdpConnectsBeforeEdit, 'saving unchanged RDP settings restarted the session')
  const resizeCountBeforeFocus = await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_RESIZES__?.length ?? 0)
  const rdpConnectsBeforeDecodeRecovery = await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0)
  await page.evaluate(() => { window.__XUNDU_SANDBOX_RDP_FAIL_NEXT_RESIZE__ = true })
  await desktopPanel.locator('.remote-session-toolbar button[title="聚焦窗口"]').click()
  await page.waitForFunction(
    (before) => (window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0) > before
      && document.querySelector('.workspace-layer.active .remote-session-remote-desktop .remote-desktop-shell')?.getAttribute('data-status') === 'connected',
    rdpConnectsBeforeDecodeRecovery,
    { timeout: 5_000 },
  )
  const focusedResize = await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_RESIZES__?.at(-1) ?? null)
  assert(
    (await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_RESIZES__?.length ?? 0)) > resizeCountBeforeFocus,
    'focusing the RDP widget did not request a dynamic resolution update',
  )
  assert(focusedResize?.width > 900 && focusedResize?.height > 500, `focused RDP resolution is too small: ${JSON.stringify(focusedResize)}`)
  assert(await desktopPanel.locator('.remote-desktop-shell').getAttribute('data-status') === 'connected', 'incomplete RDP frame recovery did not restore the session')
  assert(
    await page.evaluate(() => JSON.parse(localStorage.getItem('xundu.rdp.fixedResolutionHosts.v1') ?? '[]').includes('sandbox-rdp:3389')),
    'incomplete RDP frame did not enable fixed-resolution compatibility mode',
  )
  await desktopPanel.locator('.remote-session-toolbar button[title="退出聚焦"]').first().click()
  await page.waitForTimeout(650)
  const desktopCanvas = desktopPanel.locator('.remote-desktop-display canvas')
  await desktopCanvas.click({ button: 'right' })
  await page.waitForTimeout(80)
  assert(await page.locator('.context-menu').count() === 0, 'remote right-click leaked into the workbench context menu')
  await page.evaluate(() => {
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { readText: async () => 'sandbox clipboard text' },
    })
  })
  await desktopPanel.locator('.remote-desktop-controls button').nth(0).click()
  await page.waitForTimeout(80)
  assert((await desktopPanel.textContent())?.includes('剪贴板文本已粘贴到远程'), 'remote clipboard paste action did not run')
  assert(await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_TEXTS__?.at(-1) === 'sandbox clipboard text'), 'remote clipboard text did not reach the RDP input channel')
  await page.evaluate(() => { window.__XUNDU_SANDBOX_CLIPBOARD_FILES__ = ['C:\\sandbox\\release.zip'] })
  const uploadsBeforeDoublePaste = await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_UPLOADS__?.length ?? 0)
  await desktopPanel.locator('.remote-desktop-controls button').nth(0).evaluate((button) => {
    button.click()
    button.click()
  })
  await page.waitForTimeout(500)
  const transferProgress = desktopPanel.locator('.remote-desktop-transfer-progress')
  assert(await transferProgress.count() === 1, 'RDP file transfer did not show progress feedback')
  const transferProgressText = await transferProgress.textContent()
  assert(transferProgressText?.includes('25%'), `RDP file transfer did not report its real percentage: ${transferProgressText}`)
  assert(transferProgressText?.includes('/s') && !transferProgressText?.includes('0 B/s'), `RDP file transfer did not report network speed: ${transferProgressText}`)
  await page.screenshot({ path: 'output/playwright/rdp-file-transfer-progress.png', fullPage: false })
  await page.waitForTimeout(560)
  assert(
    await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_UPLOADS__?.at(-1)?.[0] === 'C:\\sandbox\\release.zip'),
    'copied local file did not enter the RDP file upload channel',
  )
  assert(
    await page.evaluate((before) => (window.__XUNDU_SANDBOX_RDP_UPLOADS__?.length ?? 0) === before + 1, uploadsBeforeDoublePaste),
    'rapid duplicate paste started more than one RDP file transfer',
  )
  assert((await transferProgress.textContent())?.includes('100%'), 'RDP file transfer did not reach 100%')
  await page.evaluate(() => { window.__XUNDU_SANDBOX_CLIPBOARD_FILES__ = [] })
  const desktopViewportBox = await desktopPanel.locator('.remote-desktop-viewport').boundingBox()
  assert(Boolean(desktopViewportBox), 'RDP viewport was unavailable for native file drop')
  await page.evaluate(({ x, y }) => {
    window.__XUNDU_SANDBOX_DROP_FILES__?.(['C:\\sandbox\\folder'], x, y)
  }, { x: desktopViewportBox.x + desktopViewportBox.width / 2, y: desktopViewportBox.y + desktopViewportBox.height / 2 })
  await page.waitForTimeout(60)
  assert(
    await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_UPLOADS__?.at(-1)?.[0] === 'C:\\sandbox\\folder'),
    'native file drop did not enter the RDP file upload channel',
  )
  await desktopPanel.locator('.remote-desktop-transfer-progress button[aria-label="取消文件传输"]').click()
  await page.waitForTimeout(520)
  assert((await desktopPanel.textContent())?.includes('文件传输已取消'), 'RDP file transfer cancellation did not finish cleanly')
  await desktopPanel.locator('.remote-session-toolbar button[title="聚焦窗口"]').click()
  await page.waitForTimeout(180)
  assert(await page.locator('.workspace-layer.active .wave-layout-root.has-magnified').count() === 1, 'RDP widget did not enter focus mode before add-widget regression')
  const rdpResizesBeforeAddingSibling = await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_RESIZES__?.length ?? 0)
  await page.locator('.workbench-actions button').nth(3).click()
  await page.waitForTimeout(650)
  assert(await page.locator('.workspace-layer.active .wave-layout-root.has-magnified').count() === 0, 'adding a widget left the RDP focus mode hiding the new panel')
  assert(await page.locator('.workspace-layer.active .remote-session-monitor').count() > 0, 'monitor widget was not visible after adding it beside RDP')
  assert(await desktopPanel.locator('.remote-desktop-shell').getAttribute('data-status') === 'connected', 'adding another widget disconnected RDP')
  assert(
    await page.evaluate((before) => (window.__XUNDU_SANDBOX_RDP_RESIZES__?.length ?? 0) === before, rdpResizesBeforeAddingSibling),
    'adding a sibling sent a risky shrinking RDP resolution update',
  )
  const rdpConnectsBeforeLayoutChange = await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0)
  const rdpResizesBeforeLayoutChange = await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_RESIZES__?.length ?? 0)

  for (const actionIndex of [2, 3, 4, 0]) {
    await page.locator('.workbench-actions button').nth(actionIndex).click()
    await page.waitForTimeout(120)
  }
  await page.locator('.workbench-actions button').nth(5).click()
  await page.waitForTimeout(700)
  await page.locator('.workspace-layer.active .remote-session-remote-desktop .remote-desktop-shell[data-status="connected"]').waitFor({ timeout: 3_000 })
  const rdpConnectsAfterLayoutChange = await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_CONNECTS__ ?? 0)
  assert(rdpConnectsAfterLayoutChange === rdpConnectsBeforeLayoutChange, 'layout changes restarted the RDP session')
  assert(
    await page.evaluate(() => window.__XUNDU_SANDBOX_RDP_RESIZES__?.length ?? 0) === rdpResizesBeforeLayoutChange,
    'fixed-resolution compatibility mode emitted another layout resize',
  )

  const waveGeometry = await page.evaluate(() => {
    const root = document.querySelector('.workspace-layer.active .wave-layout-root')
    if (!(root instanceof HTMLElement)) return null
    const rootBox = root.getBoundingClientRect()
    const leaves = [...root.querySelectorAll('.wave-layout-leaf')].map((leaf) => {
      const box = leaf.getBoundingClientRect()
      return { left: box.left, top: box.top, right: box.right, bottom: box.bottom }
    })
    return {
      leafCount: leaves.length,
      hasStackedBranch: Boolean(root.querySelector('.wave-layout-branch.direction-column')),
      contained: leaves.every((box) => (
        box.left >= rootBox.left - 1
        && box.top >= rootBox.top - 1
        && box.right <= rootBox.right + 1
        && box.bottom <= rootBox.bottom + 1
      )),
      overlaps: leaves.some((left, leftIndex) => leaves.some((right, rightIndex) => (
        rightIndex > leftIndex
        && Math.min(left.right, right.right) - Math.max(left.left, right.left) > 1
        && Math.min(left.bottom, right.bottom) - Math.max(left.top, right.top) > 1
      ))),
    }
  })
  assert(Boolean(waveGeometry), 'Wave layout root is missing')
  assert(waveGeometry.leafCount >= 7, `dense Wave layout has too few widgets: ${waveGeometry.leafCount}`)
  assert(waveGeometry.hasStackedBranch, 'dense Wave layout did not create a stacked branch')
  assert(waveGeometry.contained, 'Wave layout clips a widget outside the workbench')
  assert(!waveGeometry.overlaps, 'Wave layout widgets overlap each other')
  await assertTerminalCanvasFit(page, 'dense-wave')

  const divider = page.locator('.workspace-layer.active .wave-layout-divider').last()
  const dividerBox = await divider.boundingBox()
  assert(Boolean(dividerBox), 'Wave layout divider is missing')
  const dividerClass = await divider.getAttribute('class')
  const beforeDividerDrag = await page.locator('.workspace-layer.active .wave-layout-leaf').evaluateAll((leaves) => (
    leaves.map((leaf) => {
      const box = leaf.getBoundingClientRect()
      return { width: box.width, height: box.height }
    })
  ))
  await page.mouse.move(dividerBox.x + dividerBox.width / 2, dividerBox.y + dividerBox.height / 2)
  await page.mouse.down()
  await page.mouse.move(
    dividerBox.x + dividerBox.width / 2 + (dividerClass?.includes('divider-row') ? 45 : 0),
    dividerBox.y + dividerBox.height / 2 + (dividerClass?.includes('divider-column') ? 45 : 0),
    { steps: 8 },
  )
  await page.mouse.up()
  await page.waitForTimeout(500)
  const afterDividerDrag = await page.locator('.workspace-layer.active .wave-layout-leaf').evaluateAll((leaves) => (
    leaves.map((leaf) => {
      const box = leaf.getBoundingClientRect()
      return { width: box.width, height: box.height }
    })
  ))
  assert(
    afterDividerDrag.some((box, index) => (
      Math.abs(box.width - beforeDividerDrag[index].width) > 10
      || Math.abs(box.height - beforeDividerDrag[index].height) > 10
    )),
    'Wave divider drag did not resize any widget',
  )
  await assertTerminalCanvasFit(page, 'after-wave-divider-drag')
  await page.screenshot({ path: 'output/playwright/layout-wave-dark.png', fullPage: false })

  if (process.env.XUNDU_PHASE2_STRESS === '1') {
    const terminalStress = await page.evaluate(async () => {
      const bridge = await import('/src/tauriBridge.ts')
      const workspaces = JSON.parse(localStorage.getItem('xundu.workspaces.v2') ?? '[]')
      const localWidget = workspaces.flatMap((workspace) => workspace.widgets ?? []).find((widget) => widget.type === 'local-terminal')
      const startedAt = performance.now()
      const emitted = await bridge.invoke('phase2_stress_terminal_output', {
        sessionId: localWidget?.sessionId || localWidget?.id || 'local-terminal-1',
        bytes: 100 * 1024 * 1024,
      })
      return { emitted, durationMs: performance.now() - startedAt }
    })
    assert(terminalStress.emitted === 100 * 1024 * 1024, `terminal stress emitted the wrong byte count: ${JSON.stringify(terminalStress)}`)
    await page.waitForFunction(() => [...document.querySelectorAll('.remote-session-local-terminal .xterm-rows')]
      .some((element) => element.textContent?.includes('PHASE2_STRESS_DONE')), undefined, { timeout: 45_000 })
    const interactionStarted = Date.now()
    await page.locator('.dock-button[aria-label="本地"]').click()
    await page.locator('.local-tools').waitFor({ state: 'visible' })
    await page.locator('.dock-button[aria-label="本地"]').click()
    assert(Date.now() - interactionStarted < 3_000, '100 MiB terminal output left the interface unresponsive')

    await page.evaluate(() => { window.__XUNDU_SANDBOX_DIRECTORY_SIZE__ = 10_000 })
    const stressFileWidget = page.locator('.workspace-layer.active .file-widget').first()
    await stressFileWidget.locator('.file-toolbar button[title="刷新列表"]').click()
    await stressFileWidget.getByText(/已载入前 10000 项/).waitFor({ state: 'visible', timeout: 8_000 })
    const directoryStress = await stressFileWidget.evaluate((widget) => ({
      renderedRows: widget.querySelectorAll('.file-row.virtual').length,
      virtualHeight: Number.parseFloat(widget.querySelector('.file-list-virtual')?.style.height || '0'),
    }))
    assert(directoryStress.renderedRows < 100, `10,000 item directory rendered too many DOM rows: ${JSON.stringify(directoryStress)}`)
    assert(directoryStress.virtualHeight > 300_000, `10,000 item directory did not preserve virtual height: ${JSON.stringify(directoryStress)}`)
    await page.evaluate(() => { window.__XUNDU_SANDBOX_DIRECTORY_SIZE__ = 0 })

    const transferStress = await page.evaluate(async () => {
      const store = await import('/src/operationsStore.ts')
      for (let index = 0; index < 150; index += 1) {
        store.upsertTransfer({
          id: `stress-transfer-${index}`,
          title: `stress-${index}`,
          status: 'completed',
          totalBytes: 1024,
          transferredBytes: 1024,
          bytesPerSecond: 1024,
        })
      }
      store.upsertTransfer({ id: 'stress-monotonic', title: 'monotonic', status: 'running', totalBytes: 1000, transferredBytes: 800, bytesPerSecond: 20 })
      store.upsertTransfer({ id: 'stress-monotonic', transferredBytes: 200, bytesPerSecond: -5 })
      const snapshot = store.getTransfersSnapshot()
      const monotonic = snapshot.find((item) => item.id === 'stress-monotonic')
      return { count: snapshot.length, monotonic }
    })
    assert(transferStress.count === 100, `transfer history cap failed under stress: ${JSON.stringify(transferStress)}`)
    assert(transferStress.monotonic?.transferredBytes === 800 && transferStress.monotonic?.bytesPerSecond === 0, `transfer progress regressed under stress: ${JSON.stringify(transferStress)}`)

    const originalServers = await page.evaluate(() => localStorage.getItem('xundu.servers') ?? '[]')
    await page.evaluate(() => {
      const servers = Array.from({ length: 2_500 }, (_, index) => ({
        id: `bulk-server-${index}`,
        name: `bulk-server-${index}`,
        host: `10.${Math.floor(index / 65_536)}.${Math.floor(index / 256) % 256}.${index % 256}`,
        user: `operator-${index % 12}`,
        port: 22,
        group: `Bulk ${index % 25}`,
        auth: 'Agent',
      }))
      localStorage.setItem('xundu.servers', JSON.stringify(servers))
    })
    await page.reload({ waitUntil: 'domcontentloaded' })
    await page.waitForSelector('.app-shell')
    const bulkSearch = page.getByLabel('搜索 SSH 服务器和远程桌面')
    await page.evaluate(() => { window.__XUNDU_SANDBOX_APP_RENDERS__ = 0 })
    const bulkSearchStarted = Date.now()
    await bulkSearch.pressSequentially('bulk-server-2499', { delay: 2 })
    await page.locator('.global-search-result').waitFor({ state: 'visible' })
    const bulkSearchElapsed = Date.now() - bulkSearchStarted
    const bulkSearchState = await page.evaluate(() => ({
      appRenders: window.__XUNDU_SANDBOX_APP_RENDERS__ ?? 0,
      renderedResults: document.querySelectorAll('.global-search-result').length,
    }))
    assert(bulkSearchElapsed < 1_500, `2,500-profile search was too slow: ${bulkSearchElapsed}ms`)
    assert(bulkSearchState.renderedResults === 1, `2,500-profile search rendered unexpected results: ${JSON.stringify(bulkSearchState)}`)
    assert(bulkSearchState.appRenders === 0, `2,500-profile search rerendered the app: ${JSON.stringify(bulkSearchState)}`)
    await page.evaluate((payload) => localStorage.setItem('xundu.servers', payload), originalServers)
    await page.reload({ waitUntil: 'domcontentloaded' })
    await page.waitForSelector('.app-shell')
  }

  await page.evaluate(() => localStorage.setItem('xundu.displayLanguage', 'en-US'))
  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.waitForSelector('.app-shell')
  assert(await page.getByRole('button', { name: 'Task center' }).count() === 0, 'task center returned after switching to English')
  await page.locator('.dock-button[aria-label="Local"]').click()
  await page.locator('.local-tools').waitFor({ state: 'visible' })
  const englishOverflow = await page.evaluate(() => [
    ...document.querySelectorAll('.dock-button span, .local-actions button, .local-tools .info-row strong, .local-tools .info-row em'),
  ].filter((element) => element.clientWidth > 0 && element.scrollWidth > element.clientWidth + 1)
    .map((element) => ({ text: element.textContent?.trim(), client: element.clientWidth, scroll: element.scrollWidth })))
  assert(englishOverflow.length === 0, `English navigation or local panel text overflowed: ${JSON.stringify(englishOverflow)}`)
  const englishGlobalSearch = page.getByLabel('Search SSH servers and remote desktops')
  await englishGlobalSearch.fill('192.0.2.201')
  await page.locator('.global-search-result').first().waitFor({ state: 'visible' })
  await page.waitForFunction(() => document.querySelectorAll('.global-search-result').length === 1)
  assert(await page.locator('.global-search-result').count() === 1, 'English global search returned an unexpected number of results')
  const englishSearchOverflow = await page.evaluate(() => [...document.querySelectorAll('.global-search-result-copy > strong, .global-search-result-copy > em, .global-search-result-kind')]
    .filter((element) => element.clientWidth > 0 && element.scrollWidth > element.clientWidth + 1 && getComputedStyle(element).textOverflow !== 'ellipsis')
    .map((element) => element.textContent?.trim()))
  assert(englishSearchOverflow.length === 0, `English global search text overflowed: ${JSON.stringify(englishSearchOverflow)}`)
  await englishGlobalSearch.press('Escape')
  await page.getByRole('button', { name: 'Command palette' }).click()
  const englishPaletteSearch = page.getByLabel('Search commands, snippets, settings...')
  await englishPaletteSearch.fill('settings')
  await page.getByRole('button', { name: /Open settings/ }).waitFor({ state: 'visible' })
  assert(await page.locator('.palette-item').count() === 1, 'English command palette search did not match localized actions')
  const englishPaletteOverflow = await page.evaluate(() => [...document.querySelectorAll('.palette-item > span, .palette-item > strong, .palette-item > em')]
    .filter((element) => element.clientWidth > 0 && element.scrollWidth > element.clientWidth + 1 && getComputedStyle(element).textOverflow !== 'ellipsis')
    .map((element) => element.textContent?.trim()))
  assert(englishPaletteOverflow.length === 0, `English command palette text overflowed: ${JSON.stringify(englishPaletteOverflow)}`)
  await page.screenshot({ path: 'output/playwright/english-layout.png', fullPage: false })
  await page.getByRole('button', { name: 'Close command palette' }).click()
  await page.evaluate(() => localStorage.setItem('xundu.displayLanguage', 'zh-CN'))
  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.waitForSelector('.app-shell')

  const finalFilePanel = page.locator('.workspace-layer.active .file-widget').first()
  await page.evaluate(() => {
    window.__XUNDU_SANDBOX_HOST_KEY_MISMATCH__ = true
    window.__XUNDU_SANDBOX_REMOTE_LIST_SUCCESSES__ = 0
  })
  await finalFilePanel.locator('.aux-connection-trigger').click()
  await page.locator('.aux-connection-popover .aux-connection-option').filter({ hasText: '192.0.2.198' }).click()
  const hostKeyModal = page.locator('.host-key-confirmation-modal')
  await hostKeyModal.waitFor({ state: 'visible', timeout: 4_000 })
  assert(await hostKeyModal.count() === 1, 'one host key mismatch opened multiple confirmation dialogs')
  const hostKeyInlineError = await finalFilePanel.locator('.file-connection-error').textContent()
  assert(hostKeyInlineError === '服务器身份信息发生变化，已暂停连接。', `host key error was not shortened: ${hostKeyInlineError}`)
  assert(!hostKeyInlineError.includes('00:11:22:33'), 'host key fingerprint leaked into the compact file-manager error')
  assert(
    (await hostKeyModal.locator('.host-key-fingerprint code').textContent())?.startsWith('00:11:22:33'),
    'host key confirmation did not show the server fingerprint',
  )
  await page.setViewportSize({ width: 440, height: 320 })
  await page.waitForTimeout(120)
  const hostKeyModalGeometry = await hostKeyModal.evaluate((element) => {
    const box = element.getBoundingClientRect()
    return {
      left: box.left,
      top: box.top,
      right: box.right,
      bottom: box.bottom,
      viewportWidth: window.innerWidth,
      viewportHeight: window.innerHeight,
      clientWidth: element.clientWidth,
      scrollWidth: element.scrollWidth,
    }
  })
  assert(
    hostKeyModalGeometry.left >= 0
      && hostKeyModalGeometry.top >= 0
      && hostKeyModalGeometry.right <= hostKeyModalGeometry.viewportWidth
      && hostKeyModalGeometry.bottom <= hostKeyModalGeometry.viewportHeight,
    `host key confirmation overflowed the small viewport: ${JSON.stringify(hostKeyModalGeometry)}`,
  )
  assert(hostKeyModalGeometry.scrollWidth <= hostKeyModalGeometry.clientWidth + 1, `host key confirmation overflowed horizontally: ${JSON.stringify(hostKeyModalGeometry)}`)
  await page.screenshot({ path: 'output/playwright/host-key-confirmation-small.png', fullPage: false })
  await hostKeyModal.getByRole('button', { name: '确认并重试' }).click()
  await hostKeyModal.waitFor({ state: 'hidden' })
  assert(
    await page.evaluate(() => window.__XUNDU_SANDBOX_HOST_KEY_REPLACEMENTS__ === 1),
    'confirmed host key replacement was not sent to the backend',
  )
  await page.waitForFunction(() => (window.__XUNDU_SANDBOX_REMOTE_LIST_SUCCESSES__ ?? 0) > 0)
  await finalFilePanel.locator('.file-connection-error').waitFor({ state: 'hidden' })
  const remoteListSuccessesBeforeKexRetry = await page.evaluate(() => window.__XUNDU_SANDBOX_REMOTE_LIST_SUCCESSES__ ?? 0)
  await page.evaluate(() => { window.__XUNDU_SANDBOX_KEX_FAILURES_REMAINING__ = 1 })
  await finalFilePanel.getByTitle('刷新列表').click()
  const kexRetryFeedback = finalFilePanel.locator('.file-connection-error')
  await kexRetryFeedback.filter({ hasText: '正在自动重试 1/2' }).waitFor({ state: 'visible', timeout: 2_000 })
  assert(
    !(await kexRetryFeedback.textContent())?.includes('Unable to exchange encryption keys'),
    'raw SSH key-exchange failure leaked into the compact file-manager feedback',
  )
  await page.waitForFunction(
    (before) => (window.__XUNDU_SANDBOX_REMOTE_LIST_SUCCESSES__ ?? 0) > before,
    remoteListSuccessesBeforeKexRetry,
  )
  await kexRetryFeedback.waitFor({ state: 'hidden' })
  await page.setViewportSize({ width: 1482, height: 922 })

  const filePanelBox = await finalFilePanel.boundingBox()
  assert(Boolean(filePanelBox), 'remote file manager was unavailable for native file drop')
  const nativeDropPosition = {
    x: filePanelBox.x + filePanelBox.width / 2,
    y: filePanelBox.y + filePanelBox.height / 2,
  }
  await page.evaluate(({ x, y }) => {
    window.__XUNDU_SANDBOX_DRAG_FILES__?.('enter', ['C:\\sandbox\\folder'], x, y)
  }, nativeDropPosition)
  await finalFilePanel.locator('.file-manager-drop-zone').waitFor({ state: 'visible' })
  assert((await finalFilePanel.textContent())?.includes('释放后上传到当前目录'), 'native file drag did not show the upload target')
  await page.screenshot({ path: 'output/playwright/file-manager-drop-zone.png', fullPage: false })
  await page.evaluate(({ x, y }) => {
    window.__XUNDU_SANDBOX_DROP_FILES__?.([
      'C:\\sandbox\\folder',
      'C:\\sandbox\\release.zip',
      'C:\\sandbox\\checksums.txt',
    ], x, y)
  }, nativeDropPosition)
  const transferManager = page.getByRole('dialog', { name: '文件传输管理' })
  await transferManager.waitFor({ state: 'visible' })
  await transferManager.locator('.transfer-manager-item.status-completed').filter({ hasText: '3 个项目' }).waitFor({ state: 'visible', timeout: 4_000 })
  const droppedSources = await page.evaluate(() => (window.__XUNDU_SANDBOX_FILE_UPLOADS__ ?? []).at(-1))
  assert(
    droppedSources?.length === 3 && droppedSources[0].endsWith('folder'),
    `native file-manager drop did not enter recursive upload: ${JSON.stringify(droppedSources)}`,
  )
  await transferManager.getByRole('button', { name: '关闭' }).click()

  const uploadButton = finalFilePanel.getByRole('button', { name: '上传文件到当前目录' })
  const uploadsBeforePicker = await page.evaluate(() => window.__XUNDU_SANDBOX_FILE_UPLOADS__?.length ?? 0)
  await uploadButton.click()
  await transferManager.waitFor({ state: 'visible' })
  await transferManager.getByRole('button', { name: '上传' }).click()
  await page.waitForFunction((before) => (window.__XUNDU_SANDBOX_FILE_UPLOADS__?.length ?? 0) === before + 1, uploadsBeforePicker)
  await transferManager.locator('.transfer-manager-item.status-completed').filter({ hasText: '2 个项目' }).waitFor({ state: 'visible', timeout: 4_000 })
  const uploadedSources = await page.evaluate(() => (window.__XUNDU_SANDBOX_FILE_UPLOADS__ ?? []).at(-1))
  assert(uploadedSources?.length === 2 && uploadedSources[0].endsWith('release.zip'), `remote upload did not receive the selected files: ${JSON.stringify(uploadedSources)}`)
  await page.screenshot({ path: 'output/playwright/file-transfer-manager.png', fullPage: false })
  await transferManager.getByRole('button', { name: '关闭' }).click()

  await uploadButton.click()
  await transferManager.waitFor({ state: 'visible' })
  const runningUpload = transferManager.locator('.transfer-manager-item.status-running').filter({ hasText: '2 个项目' }).first()
  await runningUpload.waitFor({ state: 'visible' })
  await runningUpload.getByRole('button', { name: /取消 2 个项目/ }).click()
  await transferManager.locator('.transfer-manager-item.status-cancelled').filter({ hasText: '2 个项目' }).first().waitFor({ state: 'visible', timeout: 4_000 })
  await transferManager.getByRole('button', { name: '关闭' }).click()

  await page.locator('.titlebar button[aria-label="设置"]').click()
  await page.locator('.settings-appearance-switch button').first().click()
  await page.getByRole('button', { name: '选择图片' }).click()
  const finalTransparencySlider = page.getByRole('slider', { name: '整体透明度' })
  await finalTransparencySlider.fill('34')
  await page.locator('.settings-modal button[aria-label="关闭"]').click()
  await page.locator('.app-custom-background').waitFor({ state: 'visible' })
  await page.waitForTimeout(180)
  const internalMaterialState = await page.evaluate(() => {
    const materialElement = (element) => {
      if (!(element instanceof HTMLElement)) return { exists: false, color: '', alpha: 1 }
      const color = getComputedStyle(element).backgroundColor
      if (color === 'transparent') return { exists: true, color, alpha: 0 }
      const rgba = color.match(/rgba\([^,]+,[^,]+,[^,]+,\s*([\d.]+)\)/)
      if (rgba) return { exists: true, color, alpha: Number(rgba[1]) }
      const modern = color.match(/\/\s*([\d.]+)(%)?\s*\)$/)
      const alpha = modern ? Number(modern[1]) / (modern[2] ? 100 : 1) : 1
      return { exists: true, color, alpha }
    }
    const material = (selector) => materialElement(document.querySelector(selector))
    const fileBody = document.querySelector('.workspace-layer.active .file-widget')
    const terminalHost = document.querySelector('.workspace-layer.active .widget-terminal-host')
    return {
      titlebar: material('.titlebar'),
      dockRail: material('.dock-rail'),
      workbenchStage: material('.workbench-stage'),
      filePanel: materialElement(fileBody?.closest('.remote-session-panel, .workspace-widget') ?? fileBody),
      fileToolbar: material('.workspace-layer.active .file-toolbar'),
      monitorChart: material('.workspace-layer.active .monitor-chart'),
      processPanel: materialElement(document.querySelector('.workspace-layer.active .process-widget')?.closest('.remote-session-panel, .workspace-widget')),
      terminalPanel: materialElement(terminalHost?.closest('.remote-session-panel, .workspace-widget')),
      terminalHost: materialElement(terminalHost),
      appearance: document.documentElement.dataset.appearance,
    }
  })
  assert(internalMaterialState.appearance === 'light', `light wallpaper material regression ran under the wrong appearance: ${JSON.stringify(internalMaterialState)}`)
  assert(internalMaterialState.titlebar.exists && internalMaterialState.dockRail.exists && internalMaterialState.workbenchStage.exists, `shell material is missing: ${JSON.stringify(internalMaterialState)}`)
  assert(Math.abs(internalMaterialState.workbenchStage.alpha - internalMaterialState.titlebar.alpha) <= 0.02, `workbench transparency differs from the titlebar: ${JSON.stringify(internalMaterialState)}`)
  assert(Math.abs(internalMaterialState.workbenchStage.alpha - internalMaterialState.dockRail.alpha) <= 0.02, `workbench transparency differs from the dock rail: ${JSON.stringify(internalMaterialState)}`)
  assert(internalMaterialState.filePanel.exists && internalMaterialState.filePanel.alpha < 0.2, `file manager outer material stayed opaque: ${JSON.stringify(internalMaterialState)}`)
  assert(internalMaterialState.fileToolbar.exists && internalMaterialState.fileToolbar.alpha < 0.18, `file manager toolbar stayed opaque: ${JSON.stringify(internalMaterialState)}`)
  assert(internalMaterialState.monitorChart.exists && internalMaterialState.monitorChart.alpha < 0.18, `monitor chart stayed opaque: ${JSON.stringify(internalMaterialState)}`)
  assert(!internalMaterialState.processPanel.exists || internalMaterialState.processPanel.alpha < 0.2, `process panel stayed opaque: ${JSON.stringify(internalMaterialState)}`)
  assert(internalMaterialState.terminalPanel.exists && internalMaterialState.terminalPanel.alpha === 0, `terminal outer panel stayed opaque: ${JSON.stringify(internalMaterialState)}`)
  assert(internalMaterialState.terminalHost.exists && internalMaterialState.terminalHost.alpha < 0.15, `terminal material stayed opaque: ${JSON.stringify(internalMaterialState)}`)
  assert(Math.abs(internalMaterialState.terminalHost.alpha - internalMaterialState.filePanel.alpha) <= 0.01, `light terminal transparency differs from the file manager: ${JSON.stringify(internalMaterialState)}`)
  await assertTerminalCanvasFit(page, 'custom-background-materials')
  await page.screenshot({ path: 'output/playwright/components-custom-transparency.png', fullPage: false })

  const screenshot = await page.screenshot({ fullPage: false })
  await writeFile('smoke-sandbox.png', screenshot)

  await browser.close()

  if (pageErrors.length) {
    throw new Error(`Browser errors:\n${pageErrors.join('\n')}`)
  }

  console.log(JSON.stringify({
    ok: true,
    url,
    screenshot: 'smoke-sandbox.png',
    checks: [
      'server drawer',
      'default appearance and launch options',
      'settings appearance layout',
      'eight file-backed persistent theme presets with synchronized workspace and terminal palettes',
      'source-free theme copy and reduced-motion-safe selection feedback',
      'software about, update check, enterprise server website, and technical QQ groups',
      'optional custom background and paired interface transparency persistence',
      'translucent internal file, monitor, and process materials',
      'matched light wallpaper transparency across terminal and file-manager content',
      'terminal font size persistence',
      'SSH and RDP formatted connection import with password-safe status',
      'legacy credential migration and password-free browser persistence',
      'SSH password, private-key, and Agent profile controls',
      'simplified Run, saved commands, recent history, and notes drawers',
      'Run drawer dispatches commands to the focused terminal',
      'local and SSH AI CLI detection and one-click launchers',
      'real CLI brand logo assets without letter placeholders',
      'interactive SSH menu prompt remains visible on the final terminal row',
      'terminal scrollback copy and drawer-safe toolbar actions',
      'terminal right-click copy/paste and selection-aware Ctrl+C',
      'independent right-click and Ctrl+C copy in the second SSH terminal',
      'light file-editor palette',
      'fresh file read on every editor open',
      'file manager context actions and confirmation dialogs',
      'editable file path navigation and local drive discovery',
      'file download progress, completion, and cancellation',
      'unified upload/download transfer manager with SFTP upload progress and cancellation',
      'native multi-file and recursive folder drop into the remote file manager',
      'automatic SSH host key confirmation, small-window containment, and safe replacement retry',
      'automatic file-channel KEX retry with compact Chinese feedback',
      'unified transfer history with credential-free persistence',
      'task center removal without transfer regression',
      'isolated global SSH/RDP search with keyboard navigation',
      'responsive local sidebar and command palette',
      'localized command search and English overflow safety',
      'deprecated recovery state cleanup with no recovery UI',
      'wrapped local-terminal backspace',
      'context menu aux widgets',
      'zero SSH/RDP connections on startup',
      'single-click SSH selection and double-click direct connect',
      'sidebar remote desktop profile management',
      'manual RDP standby and connect',
      'mock SSH stream',
      'sub-120ms sandbox SSH single-character echo path',
      'per-server connection state',
      'all active terminals use live xterm renderers',
      'Chinese IME commit on a background terminal',
      'long IME preedit wraps and remains editable',
      'Shift IME switch preserves raw ASCII preedit',
      'workspace terminal persistence',
      'workspace file-manager path persistence',
      'workspace tab close lifecycle',
      'Native RDP NLA-to-TLS fallback, render, and persistence',
      'live RDP edit cancel and unchanged-save session preservation',
      'RDP incomplete-frame recovery and fixed-resolution compatibility fallback',
      'remote right-click isolation and clipboard paste',
      'RDP clipboard upload, native drop, progress, speed, deduplication, and cancellation',
      'RDP focus exits when adding another workbench widget',
      'RDP session persistence across layout changes',
      'non-blocking terminal refresh',
      'stable refresh identity and lock',
      'dense Wave layout geometry',
      'Wave divider resize',
      'theme-aware terminal palette',
      'solid material rendering',
      'multi-panel canvas containment',
      'compact terminal inset without wide gray gutters',
    ],
  }, null, 2))
} finally {
  stopServer()
}

async function launchBrowser() {
  const errors = []
  for (const channel of ['chrome', 'msedge']) {
    try {
      return await chromium.launch({ channel, headless: true })
    } catch (error) {
      errors.push(`${channel}: ${String(error).split('\n')[0]}`)
    }
  }
  try {
    return await chromium.launch({ headless: true })
  } catch (error) {
    errors.push(`bundled: ${String(error).split('\n')[0]}`)
  }
  throw new Error(`No Playwright browser available:\n${errors.join('\n')}`)
}

async function ensureServerDrawer(page) {
  if (await page.locator('.server-source-list .source-section:first-child .server-item').count()) return
  await page.locator('.dock-button').nth(0).click()
  await page.waitForTimeout(250)
}

async function expectCount(page, selector, expected, label) {
  const count = await page.locator(selector).count()
  assert(count === expected, `${label}: expected ${expected}, got ${count}`)
}

async function pasteConnectionImport(locator, text) {
  await locator.evaluate((element, value) => {
    const clipboardData = new DataTransfer()
    clipboardData.setData('text/plain', value)
    element.dispatchEvent(new ClipboardEvent('paste', {
      bubbles: true,
      cancelable: true,
      clipboardData,
    }))
  }, text)
}

async function waitForServer(targetUrl) {
  const started = Date.now()
  let lastError = ''
  while (Date.now() - started < 30_000) {
    try {
      const response = await fetch(targetUrl)
      if (response.ok) return
      lastError = `HTTP ${response.status}`
    } catch (error) {
      lastError = String(error)
    }
    await new Promise((resolve) => setTimeout(resolve, 300))
  }
  throw new Error(`Sandbox server did not start: ${lastError}\n${serverLog}`)
}

function assert(condition, message) {
  if (!condition) throw new Error(message)
}

function countCharacter(value, character) {
  return [...(value ?? '')].filter((current) => current === character).length
}

async function assertTerminalCanvasFit(page, label) {
  const metrics = await page.evaluate(() => [...document.querySelectorAll('.workspace-layer.active .widget-terminal-host')].map((host) => {
    const hostBox = host.getBoundingClientRect()
    const xterm = host.querySelector('.xterm')
    const xtermBox = xterm?.getBoundingClientRect()
    const screen = host.querySelector('.xterm-screen')
    const screenBox = screen?.getBoundingClientRect()
    const canvasBoxes = [...host.querySelectorAll('.xterm-screen canvas')].map((canvas) => canvas.getBoundingClientRect())
    return {
      hostWidth: hostBox.width,
      hostHeight: hostBox.height,
      hostLeft: hostBox.left,
      hostTop: hostBox.top,
      hostRight: hostBox.right,
      hostBottom: hostBox.bottom,
      xtermLeft: xtermBox?.left ?? 0,
      xtermTop: xtermBox?.top ?? 0,
      xtermRight: xtermBox?.right ?? 0,
      xtermBottom: xtermBox?.bottom ?? 0,
      screenWidth: screenBox?.width ?? 0,
      screenHeight: screenBox?.height ?? 0,
      screenLeft: screenBox?.left ?? 0,
      screenBottom: screenBox?.bottom ?? 0,
      canvasesInside: canvasBoxes.every((box) => box.left >= hostBox.left - 1 && box.right <= hostBox.right + 1),
    }
  }))
  assert(metrics.length >= 1, `${label}: no live terminal renderer is available`)
  for (const metric of metrics) {
    const detail = JSON.stringify(metric)
    assert(metric.screenWidth > 0 && metric.screenWidth <= metric.hostWidth + 1, `${label}: xterm screen width escaped its host ${detail}`)
    assert(metric.screenHeight > 0 && metric.screenHeight <= metric.hostHeight + 1, `${label}: xterm screen height escaped its host ${detail}`)
    assert(metric.xtermLeft - metric.hostLeft >= 9 && metric.xtermLeft - metric.hostLeft <= 11, `${label}: xterm left inset is not compact ${detail}`)
    assert(metric.xtermTop - metric.hostTop >= 7 && metric.xtermTop - metric.hostTop <= 9, `${label}: xterm top inset is not compact ${detail}`)
    assert(metric.hostRight - metric.xtermRight >= 9 && metric.hostRight - metric.xtermRight <= 11, `${label}: xterm right inset is not compact ${detail}`)
    assert(metric.hostBottom - metric.xtermBottom >= 11 && metric.hostBottom - metric.xtermBottom <= 13, `${label}: xterm bottom inset is not compact ${detail}`)
    assert(metric.screenLeft - metric.hostLeft >= 8 && metric.screenLeft - metric.hostLeft <= 12, `${label}: xterm left inset is not compact ${detail}`)
    assert(metric.hostBottom - metric.screenBottom >= 11 && metric.hostBottom - metric.screenBottom <= 32, `${label}: xterm bottom inset is not compact ${detail}`)
    assert(metric.canvasesInside, `${label}: xterm canvas escaped its host ${detail}`)
  }
}

function stopServer() {
  if (process.platform === 'win32' && server.pid) {
    spawnSync('taskkill', ['/PID', String(server.pid), '/T', '/F'], { stdio: 'ignore' })
    return
  }
  server.kill()
}
