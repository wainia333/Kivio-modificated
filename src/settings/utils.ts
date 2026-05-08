const modifierKeys = new Set(['Shift', 'Meta', 'Control', 'Alt', 'AltGraph'])

const keyAliasMap: Record<string, string> = {
  Escape: 'Esc',
  ' ': 'Space',
  Spacebar: 'Space',
  ArrowUp: 'Up',
  ArrowDown: 'Down',
  ArrowLeft: 'Left',
  ArrowRight: 'Right',
}

const normalizeKeyFromCode = (code: string) => {
  if (code.startsWith('Key')) return code.slice(3)
  if (code.startsWith('Digit')) return code.slice(5)
  return ''
}

export const normalizeHotkeyKey = (event: KeyboardEvent) => {
  const { key, code } = event
  if (!key) return ''
  if (modifierKeys.has(key)) return ''
  if (/^F\d{1,2}$/.test(key)) return key.toUpperCase()
  const alias = keyAliasMap[key]
  if (alias) return alias
  const fromCode = normalizeKeyFromCode(code)
  if (fromCode) return fromCode.toUpperCase()
  if (key === 'Dead' || key === 'Process') return ''
  if (key.length === 1 && key !== '+') return key.toUpperCase()
  return ''
}

export const buildHotkey = (event: KeyboardEvent) => {
  const key = normalizeHotkeyKey(event)
  if (!key) return ''
  const parts: string[] = []
  if (event.metaKey || event.ctrlKey) parts.push('CommandOrControl')
  if (event.altKey || event.getModifierState('AltGraph')) parts.push('Alt')
  if (event.shiftKey) parts.push('Shift')
  parts.push(key)
  return parts.join('+')
}

export const getPlatform = () => {
  if (navigator.platform.startsWith('Mac')) return 'macos'
  if (navigator.platform.startsWith('Win')) return 'windows'
  return 'linux'
}

export const formatHotkey = (hotkey: string, platform: 'macos' | 'windows' | 'linux'): string[] => {
  const parts = hotkey.split('+')
  return parts.map((part) => {
    switch (part) {
      case 'CommandOrControl':
        return platform === 'macos' ? '⌘' : 'Ctrl'
      case 'Command':
        return '⌘'
      case 'Control':
        return 'Ctrl'
      case 'Alt':
        return platform === 'macos' ? '⌥' : 'Alt'
      case 'Shift':
        return platform === 'macos' ? '⇧' : 'Shift'
      case 'Escape':
        return 'Esc'
      case 'Space':
        return 'Space'
      case 'ArrowUp':
        return '↑'
      case 'ArrowDown':
        return '↓'
      case 'ArrowLeft':
        return '←'
      case 'ArrowRight':
        return '→'
      default:
        return part.length === 1 ? part.toUpperCase() : part
    }
  })
}
