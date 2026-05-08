import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getVersion } from '@tauri-apps/api/app'
import { getCurrentWindow, LogicalSize } from '@tauri-apps/api/window'

export type ExplainMessage = { role: 'user' | 'assistant'; content: string; reasoning?: string }

export type LensStreamPayload = {
  imageId: string
  kind: 'answer'
  delta: string
  reasoningDelta?: string
  done?: boolean
  reason?: 'done' | 'cancelled' | 'error'
  full?: string
}

export type LensTranslateStreamPayload = {
  imageId: string
  kind?: 'original' | 'translated'
  delta?: string
  done?: boolean
  success?: boolean
  error?: string | null
}

export type LensWindowInfo = {
  id: number
  owner: string
  title: string
  x: number
  y: number
  width: number
  height: number
}

export type LensCursorPosition = {
  x: number
  y: number
}

export type ModelProvider = {
  id: string
  name: string
  apiKeys: string[]
  baseUrl: string
  availableModels: string[]
  enabledModels: string[]
}

export type ProviderConnectionInput = {
  id?: string
  baseUrl: string
  apiKeys: string[]
}

export type Settings = {
  hotkey: string
  theme: 'system' | 'light' | 'dark'
  targetLang: string
  source: string
  autoPaste: boolean
  launchAtStartup: boolean
  translatorProviderId: string
  translatorModel: string
  translatorPrompt?: string
  providers: ModelProvider[]
  retryEnabled: boolean
  retryAttempts: number
  screenshotTranslation: {
    enabled: boolean
    hotkey: string
    providerId: string
    model: string
    ocrMethod?: 'ai' | 'baidu' | 'chaoxing' | 'system'
    translationMethod?: 'ai' | 'baidu' | 'google' | 'tencent' | 'bing' | 'bing2' | 'yandex' | 'caiyun2' | 'microsoft'
    translateProviderId?: string
    translateModel?: string
    baiduOcr?: {
      apiKey: string
      secretKey: string
      languageType?: string
      accurate?: boolean
    }
    baiduTranslate?: {
      appId: string
      appKey: string
      sourceLang?: string
    }
    tencentTranslate?: {
      secretId: string
      secretKey: string
    }
    caiyunTranslate?: {
      token: string
    }
    directTranslate?: boolean
    thinkingEnabled?: boolean
    thinkingEffort?: 'low' | 'medium' | 'high' | 'xhigh'
    streamEnabled?: boolean
    keepFullscreenAfterCapture?: boolean
    useSystemOcr?: boolean
    ocrPrompt?: string
    prompt?: string
  }
  lens: {
    enabled: boolean
    hotkey: string
    providerId?: string
    model?: string
    defaultLanguage?: string
    streamEnabled?: boolean
    thinkingEnabled?: boolean
    thinkingEffort?: 'low' | 'medium' | 'high' | 'xhigh'
    webSearchEnabled?: boolean
    systemPrompt?: string
    questionPrompt?: string
    messageOrder?: 'asc' | 'desc'
    keepFullscreenAfterCapture?: boolean
  }
  promptOptimizer: {
    enabled: boolean
    hotkey: string
    providerId?: string
    model?: string
    defaultLanguage?: string
    systemPrompt?: string
    optimizePrompt?: string
  }
  settingsLanguage?: 'zh' | 'en'
  autoCheckUpdate?: boolean
  imageArchiveEnabled?: boolean
  imageArchivePath?: string
}

export type UpdateInfo = {
  available: boolean
  version?: string
  tag?: string
  htmlUrl?: string
  body?: string
  publishedAt?: string
}

export type DefaultPromptTemplates = {
  translationTemplate: string
  screenshotOcrPrompt?: string
  screenshotTranslationTemplate?: string
  lensPrompts: {
    zh: { system: string; question: string }
    en: { system: string; question: string }
  }
  promptOptimizerPrompts?: {
    zh: { system: string; optimize: string }
    en: { system: string; optimize: string }
  }
}

export type PermissionStatus = {
  platform: 'macos' | 'other'
  accessibility: boolean
  screenRecording: boolean
}

type Unlisten = () => void

async function on<T>(event: string, handler: (payload: T) => void): Promise<Unlisten> {
  const unlisten = await listen<T>(event, (event) => handler(event.payload))
  return () => {
    unlisten()
  }
}

export const api = {
  getSettings: () => invoke<Settings>('get_settings'),
  getDefaultPromptTemplates: () => invoke<DefaultPromptTemplates>('get_default_prompt_templates'),
  saveSettings: (settings: Settings) => invoke<void>('save_settings', { settings }),
  exportSettingsConfig: () => invoke<boolean>('export_settings_config'),
  importSettingsConfig: () => invoke<Settings | null>('import_settings_config'),

  fetchModels: (providerId: string, provider?: ProviderConnectionInput) =>
    invoke<string[]>('fetch_models', { providerId, provider }),
  testProviderConnection: (providerId: string, provider?: ProviderConnectionInput) =>
    invoke<{ success: boolean; error?: string }>('test_provider_connection', { providerId, provider }),

  getPermissionStatus: () => invoke<PermissionStatus>('get_permission_status'),
  openPermissionSettings: (kind: 'accessibility' | 'screen-recording') =>
    invoke<void>('open_permission_settings', { kind }),

  getAppVersion: () => getVersion(),

  translateText: (text: string) => invoke<string>('translate_text', { text }),
  optimizePrompt: (text: string) => invoke<string>('optimize_prompt', { text }),
  commitTranslation: (text: string) => invoke<void>('commit_translation', { text }),
  takeTranslatorSelection: () => invoke<string>('take_translator_selection'),

  openExternal: (url: string) => invoke<void>('open_external', { url }),

  resizeWindow: async (width: number, height: number) => {
    const win = getCurrentWindow()
    await win.setSize(new LogicalSize(width, height))
  },
  hideWindow: async () => {
    const win = getCurrentWindow()
    await win.hide()
  },
  closeWindow: async () => {
    const win = getCurrentWindow()
    await win.hide()
  },
  showWindow: async () => {
    const win = getCurrentWindow()
    await win.show()
  },
  startDragging: async () => {
    const win = getCurrentWindow()
    await win.startDragging()
  },
  setAlwaysOnTop: async (alwaysOnTop: boolean) => {
    const win = getCurrentWindow()
    await win.setAlwaysOnTop(alwaysOnTop)
  },

  onOpenSettings: (listener: () => void) => on('open-settings', () => listener()),

  explainReadImage: (imageId: string) =>
    invoke<{ success: boolean; data?: string; error?: string }>('explain_read_image', { imageId }),

  onLensStream: (listener: (payload: LensStreamPayload) => void) =>
    on<LensStreamPayload>('lens-stream', (payload) => listener(payload)),
  onLensTranslateStream: (listener: (payload: LensTranslateStreamPayload) => void) =>
    on<LensTranslateStreamPayload>('lens-translate-stream', (payload) => listener(payload)),
  lensRequest: () => invoke<void>('lens_request'),
  lensCursorPosition: () => invoke<LensCursorPosition | null>('lens_cursor_position'),
  lensListWindows: () => invoke<LensWindowInfo[]>('lens_list_windows'),
  lensCaptureWindow: (windowId: number) =>
    invoke<{ success: boolean; imageId?: string; error?: string }>('lens_capture_window', { windowId }),
  lensCaptureRegion: (params: {
    absoluteX: number
    absoluteY: number
    x: number
    y: number
    width: number
    height: number
    scaleFactor: number
  }) => invoke<{ success: boolean; imageId?: string; error?: string }>('lens_capture_region', params),
  lensRegisterAnnotatedImage: (base64Png: string) =>
    invoke<{ success: boolean; imageId?: string; error?: string }>(
      'lens_register_annotated_image', { base64Png }
    ),
  lensRequestTranslate: () => invoke<void>('lens_request_translate'),
  lensTranslate: (imageId: string) =>
    invoke<{ success: boolean; original?: string; translated?: string; error?: string }>(
      'lens_translate', { imageId }
    ),
  lensTranslateText: (text: string) =>
    invoke<{ success: boolean; translated?: string; error?: string }>(
      'lens_translate_text', { text }
    ),
  synthesizeSpeech: (text: string) =>
    invoke<{ success: boolean; data?: string; error?: string }>(
      'synthesize_speech', { text }
    ),
  lensAsk: (imageId: string, messages: ExplainMessage[]) =>
    invoke<{ success: boolean; response?: string; error?: string }>('lens_ask', { imageId, messages }),
  lensCancelStream: () => invoke<void>('lens_cancel_stream'),
  lensClose: () => invoke<void>('lens_close'),
  lensCommitImageToHistory: (imageId: string) =>
    invoke<void>('lens_commit_image_to_history', { imageId }),
  lensDeleteHistoryImage: (imageId: string) =>
    invoke<void>('lens_delete_history_image', { imageId }),
  lensSetFloating: (rect: {
    x?: number
    y?: number
    width: number
    height: number
    hitRegion?: { x: number; y: number; width: number; height: number } | null
  }) =>
    invoke<void>('lens_set_floating', { rect }),
  lensFlyFloating: (rect: {
    from: { x: number; y: number }
    to: { x: number; y: number }
    width: number
    height: number
    durationMs?: number
  }) =>
    invoke<void>('lens_fly_floating', { rect }),
  lensSetHitRegion: (rect: { x: number; y: number; width: number; height: number } | null) =>
    invoke<boolean>('lens_set_hit_region', { rect }),
  lensSetIgnoreCursorEvents: (ignore: boolean) =>
    invoke<void>('lens_set_ignore_cursor_events', { ignore }),
  takeLensSelection: () => invoke<string>('take_lens_selection'),

  checkUpdate: () => invoke<UpdateInfo>('check_github_latest_release'),

  appleIntelligenceAvailable: () => invoke<boolean>('apple_intelligence_available'),

  downloadUpdate: (version: string) => invoke<string>('download_update_asset', { version }),

  installUpdate: (path: string) => invoke<void>('install_update_and_quit', { path }),

  onUpdateDownloadProgress: (
    listener: (p: { percent: number; downloadedBytes: number; totalBytes: number }) => void,
  ) => on<{ percent: number; downloadedBytes: number; totalBytes: number }>(
    'update-download-progress',
    (payload) => listener(payload),
  ),

  onUpdateAvailable: (listener: (info: UpdateInfo) => void) =>
    on<UpdateInfo>('update-available', (payload) => listener(payload)),
}
