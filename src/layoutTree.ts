export type LayoutDirection = 'row' | 'column'

export type LayoutDropZone =
  | 'inline-before'
  | 'inline-after'
  | 'outer-before'
  | 'inner-before'
  | 'inner-after'
  | 'outer-after'
  | 'swap'

export type LayoutLeaf = {
  id: string
  type: 'leaf'
  widgetId: string
  size: number
}

export type LayoutBranch = {
  id: string
  type: 'branch'
  direction: LayoutDirection
  size: number
  children: LayoutNode[]
}

export type LayoutNode = LayoutLeaf | LayoutBranch

const MIN_NODE_SIZE = 0.05

function createNodeId(prefix: 'leaf' | 'branch') {
  return `${prefix}-${crypto.randomUUID()}`
}

export function createLayoutLeaf(widgetId: string, size = 1): LayoutLeaf {
  return {
    id: createNodeId('leaf'),
    type: 'leaf',
    widgetId,
    size,
  }
}

function createLayoutBranch(
  direction: LayoutDirection,
  children: LayoutNode[],
  size = 1,
): LayoutBranch {
  return {
    id: createNodeId('branch'),
    type: 'branch',
    direction,
    size,
    children,
  }
}

function oppositeDirection(direction: LayoutDirection): LayoutDirection {
  return direction === 'row' ? 'column' : 'row'
}

function normalizeSize(value: unknown, fallback = 1) {
  const parsed = Number(value)
  return Number.isFinite(parsed) && parsed > 0 ? Math.max(MIN_NODE_SIZE, parsed) : fallback
}

function normalizeNode(node: LayoutNode | null): LayoutNode | null {
  if (!node) return null
  if (node.type === 'leaf') {
    return { ...node, size: normalizeSize(node.size) }
  }

  const children = node.children
    .map((child) => normalizeNode(child))
    .filter((child): child is LayoutNode => Boolean(child))

  if (children.length === 0) return null
  if (children.length === 1) {
    return { ...children[0], size: normalizeSize(node.size) }
  }

  return {
    ...node,
    size: normalizeSize(node.size),
    children,
  }
}

function withRootSize(node: LayoutNode | null): LayoutNode | null {
  return node ? { ...node, size: 1 } : null
}

export function createLayoutForWidgets(widgetIds: string[]): LayoutNode | null {
  return widgetIds.reduce<LayoutNode | null>((layout, widgetId) => addWidgetToLayout(layout, widgetId), null)
}

export function listLayoutWidgetIds(node: LayoutNode | null): string[] {
  if (!node) return []
  if (node.type === 'leaf') return [node.widgetId]
  return node.children.flatMap((child) => listLayoutWidgetIds(child))
}

export function layoutContainsWidget(node: LayoutNode | null, widgetId: string): boolean {
  if (!node) return false
  if (node.type === 'leaf') return node.widgetId === widgetId
  return node.children.some((child) => layoutContainsWidget(child, widgetId))
}

export function addWidgetToLayout(layout: LayoutNode | null, widgetId: string): LayoutNode {
  if (layoutContainsWidget(layout, widgetId)) return layout!
  const leaf = createLayoutLeaf(widgetId)
  if (!layout) return leaf
  if (layout.type === 'leaf') {
    return createLayoutBranch('row', [
      { ...layout, size: 1 },
      leaf,
    ])
  }

  const root = cloneLayout(layout)
  if (root.type !== 'branch') return root

  if (root.children.length < 5) {
    root.children.push(leaf)
    return withRootSize(root)!
  }

  const targetIndex = findAutoInsertTarget(root)
  const target = root.children[targetIndex]
  if (target.type === 'branch' && target.direction === 'column' && target.children.length < 5) {
    target.children.push(leaf)
  } else {
    root.children[targetIndex] = createLayoutBranch(
      'column',
      [
        { ...target, size: 1 },
        leaf,
      ],
      target.size,
    )
  }
  return withRootSize(root)!
}

function findAutoInsertTarget(root: LayoutBranch) {
  for (let index = root.children.length - 1; index >= 0; index -= 1) {
    const child = root.children[index]
    if (child.type === 'leaf') return index
    if (child.direction === 'column' && child.children.length < 5) return index
  }
  return root.children.length - 1
}

export function removeWidgetFromLayout(layout: LayoutNode | null, widgetId: string): LayoutNode | null {
  const [next] = extractWidget(layout, widgetId)
  return withRootSize(normalizeNode(next))
}

function extractWidget(
  node: LayoutNode | null,
  widgetId: string,
): [LayoutNode | null, LayoutLeaf | null] {
  if (!node) return [null, null]
  if (node.type === 'leaf') {
    return node.widgetId === widgetId
      ? [null, { ...node, size: 1 }]
      : [node, null]
  }

  for (let index = 0; index < node.children.length; index += 1) {
    const [nextChild, extracted] = extractWidget(node.children[index], widgetId)
    if (!extracted) continue
    const children = [...node.children]
    if (nextChild) children[index] = nextChild
    else children.splice(index, 1)
    return [normalizeNode({ ...node, children }), extracted]
  }
  return [node, null]
}

export function ensureLayoutWidgets(
  layout: LayoutNode | null,
  widgetIds: string[],
): LayoutNode | null {
  const allowed = new Set(widgetIds)
  const seen = new Set<string>()

  function prune(node: LayoutNode | null): LayoutNode | null {
    if (!node) return null
    if (node.type === 'leaf') {
      if (!allowed.has(node.widgetId) || seen.has(node.widgetId)) return null
      seen.add(node.widgetId)
      return node
    }
    return normalizeNode({
      ...node,
      children: node.children
        .map((child) => prune(child))
        .filter((child): child is LayoutNode => Boolean(child)),
    })
  }

  let next = withRootSize(prune(layout))
  widgetIds.forEach((widgetId) => {
    if (!seen.has(widgetId)) next = addWidgetToLayout(next, widgetId)
  })
  return withRootSize(next)
}

export function moveWidgetInLayout(
  layout: LayoutNode | null,
  sourceWidgetId: string,
  targetWidgetId: string,
  zone: LayoutDropZone,
  parentDirection: LayoutDirection,
): LayoutNode | null {
  if (!layout || sourceWidgetId === targetWidgetId) return layout
  if (!layoutContainsWidget(layout, sourceWidgetId) || !layoutContainsWidget(layout, targetWidgetId)) {
    return layout
  }

  if (zone === 'swap') {
    return withRootSize(swapWidgets(cloneLayout(layout), sourceWidgetId, targetWidgetId))
  }

  const [withoutSource, extracted] = extractWidget(cloneLayout(layout), sourceWidgetId)
  if (!withoutSource || !extracted) return layout
  const source = { ...extracted, size: 1 }

  if (zone === 'inline-before' || zone === 'inline-after') {
    const [next, inserted] = insertInline(
      withoutSource,
      targetWidgetId,
      source,
      zone === 'inline-before',
      parentDirection,
    )
    return inserted ? withRootSize(normalizeNode(next)) : layout
  }

  if (zone === 'inner-before' || zone === 'inner-after') {
    const [next, inserted] = insertInner(
      withoutSource,
      targetWidgetId,
      source,
      zone === 'inner-before',
      oppositeDirection(parentDirection),
    )
    return inserted ? withRootSize(normalizeNode(next)) : layout
  }

  return insertOuter(
    withoutSource,
    targetWidgetId,
    source,
    zone === 'outer-before',
    oppositeDirection(parentDirection),
  ) ?? layout
}

function insertInline(
  node: LayoutNode,
  targetWidgetId: string,
  source: LayoutLeaf,
  before: boolean,
  fallbackDirection: LayoutDirection,
): [LayoutNode, boolean] {
  if (node.type === 'leaf') {
    if (node.widgetId !== targetWidgetId) return [node, false]
    return [
      createLayoutBranch(
        fallbackDirection,
        before ? [source, { ...node, size: 1 }] : [{ ...node, size: 1 }, source],
        node.size,
      ),
      true,
    ]
  }

  const directIndex = node.children.findIndex(
    (child) => child.type === 'leaf' && child.widgetId === targetWidgetId,
  )
  if (directIndex >= 0) {
    const children = [...node.children]
    const target = children[directIndex]
    const splitSize = Math.max(MIN_NODE_SIZE, target.size / 2)
    children[directIndex] = { ...target, size: splitSize }
    children.splice(before ? directIndex : directIndex + 1, 0, { ...source, size: splitSize })
    return [{ ...node, children }, true]
  }

  for (let index = 0; index < node.children.length; index += 1) {
    const [nextChild, inserted] = insertInline(
      node.children[index],
      targetWidgetId,
      source,
      before,
      fallbackDirection,
    )
    if (!inserted) continue
    const children = [...node.children]
    children[index] = nextChild
    return [{ ...node, children }, true]
  }
  return [node, false]
}

function insertInner(
  node: LayoutNode,
  targetWidgetId: string,
  source: LayoutLeaf,
  before: boolean,
  direction: LayoutDirection,
): [LayoutNode, boolean] {
  if (node.type === 'leaf') {
    if (node.widgetId !== targetWidgetId) return [node, false]
    const target = { ...node, size: 1 }
    return [
      createLayoutBranch(direction, before ? [source, target] : [target, source], node.size),
      true,
    ]
  }

  for (let index = 0; index < node.children.length; index += 1) {
    const [nextChild, inserted] = insertInner(
      node.children[index],
      targetWidgetId,
      source,
      before,
      direction,
    )
    if (!inserted) continue
    const children = [...node.children]
    children[index] = nextChild
    return [{ ...node, children }, true]
  }
  return [node, false]
}

function insertOuter(
  root: LayoutNode,
  targetWidgetId: string,
  source: LayoutLeaf,
  before: boolean,
  direction: LayoutDirection,
): LayoutNode | null {
  const cloned = cloneLayout(root)
  const path = findWidgetPath(cloned, targetWidgetId)
  if (!path) return null

  if (path.length >= 3) {
    const targetParent = path[path.length - 2]
    const grandParent = path[path.length - 3]
    if (grandParent.type === 'branch') {
      const targetIndex = grandParent.children.findIndex((child) => child.id === targetParent.id)
      if (targetIndex >= 0) {
        const splitSize = Math.max(MIN_NODE_SIZE, targetParent.size / 2)
        grandParent.children[targetIndex] = { ...targetParent, size: splitSize }
        grandParent.children.splice(
          before ? targetIndex : targetIndex + 1,
          0,
          { ...source, size: splitSize },
        )
        return withRootSize(normalizeNode(cloned))
      }
    }
  }

  return withRootSize(createLayoutBranch(
    direction,
    before ? [source, { ...cloned, size: 1 }] : [{ ...cloned, size: 1 }, source],
  ))
}

function findWidgetPath(node: LayoutNode, widgetId: string, path: LayoutNode[] = []): LayoutNode[] | null {
  const nextPath = [...path, node]
  if (node.type === 'leaf') return node.widgetId === widgetId ? nextPath : null
  for (const child of node.children) {
    const match = findWidgetPath(child, widgetId, nextPath)
    if (match) return match
  }
  return null
}

function swapWidgets(node: LayoutNode, sourceWidgetId: string, targetWidgetId: string): LayoutNode {
  if (node.type === 'leaf') {
    if (node.widgetId === sourceWidgetId) return { ...node, widgetId: targetWidgetId }
    if (node.widgetId === targetWidgetId) return { ...node, widgetId: sourceWidgetId }
    return node
  }
  return { ...node, children: node.children.map((child) => swapWidgets(child, sourceWidgetId, targetWidgetId)) }
}

export function resizeLayoutBranch(
  layout: LayoutNode | null,
  branchId: string,
  beforeIndex: number,
  beforeSize: number,
  afterSize: number,
): LayoutNode | null {
  if (!layout) return layout
  if (layout.type === 'leaf') return layout

  if (layout.id === branchId) {
    if (!layout.children[beforeIndex] || !layout.children[beforeIndex + 1]) return layout
    const children = [...layout.children]
    children[beforeIndex] = { ...children[beforeIndex], size: normalizeSize(beforeSize) }
    children[beforeIndex + 1] = { ...children[beforeIndex + 1], size: normalizeSize(afterSize) }
    return { ...layout, children }
  }

  return {
    ...layout,
    children: layout.children.map((child) =>
      resizeLayoutBranch(child, branchId, beforeIndex, beforeSize, afterSize) ?? child,
    ),
  }
}

export function parseLayoutNode(value: unknown, allowedWidgetIds: string[]): LayoutNode | null {
  const allowed = new Set(allowedWidgetIds)

  function parseNode(source: unknown): LayoutNode | null {
    if (!source || typeof source !== 'object') return null
    const record = source as Record<string, unknown>
    const size = normalizeSize(record.size)
    if (record.type === 'leaf' && typeof record.widgetId === 'string' && allowed.has(record.widgetId)) {
      return {
        id: typeof record.id === 'string' && record.id ? record.id : createNodeId('leaf'),
        type: 'leaf',
        widgetId: record.widgetId,
        size,
      }
    }
    if (record.type !== 'branch' || !Array.isArray(record.children)) return null
    const children = record.children
      .map((child) => parseNode(child))
      .filter((child): child is LayoutNode => Boolean(child))
    if (children.length === 0) return null
    return {
      id: typeof record.id === 'string' && record.id ? record.id : createNodeId('branch'),
      type: 'branch',
      direction: record.direction === 'column' ? 'column' : 'row',
      size,
      children,
    }
  }

  return ensureLayoutWidgets(parseNode(value), allowedWidgetIds)
}

export function cloneLayout(node: LayoutNode): LayoutNode {
  if (node.type === 'leaf') return { ...node }
  return { ...node, children: node.children.map((child) => cloneLayout(child)) }
}
