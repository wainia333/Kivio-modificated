import { lazy, Suspense, useState, useEffect, useRef, useCallback } from 'react'
import { Settings as SettingsIcon, Cpu, Loader2 } from 'lucide-react'
import { api, type Settings } from './api/tauri'
import { i18n, type Lang } from './settings/i18n'
import './index.css'

const Settings = lazy(() => import('./Settings'))
const Lens = lazy(() => import('./Lens'))

type TranslationMethod = NonNullable<Settings['screenshotTranslation']['translationMethod']>

const TRANSLATOR_WINDOW_W = 600
const TRANSLATOR_WINDOW_H = 420
const SETTINGS_WINDOW_W = 640
const SETTINGS_WINDOW_H = 520

/**
 * 翻译器主组件
 * 磨砂玻璃风格悬浮窗：顶部 drag bar、输入与结果分层级、底部提示与模型芯片。
 */
function Translator({
  translateSource,
  lang,
  onOpenSettings,
}: {
  translateSource: string
  lang: Lang
  onOpenSettings: () => void
}) {
  const [input, setInput] = useState('')
  const [result, setResult] = useState('')
  const [loading, setLoading] = useState(false)
  const [translationMethod, setTranslationMethod] = useState<TranslationMethod>('ai')
  const [methodSwitching, setMethodSwitching] = useState(false)
  const resultRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLTextAreaElement>(null)
  const translateSeq = useRef(0)
  const skipNextDebouncedInputRef = useRef<string | null>(null)
  const t = i18n[lang]
  const translationMethodOptions: { value: TranslationMethod; label: string }[] = [
    { value: 'ai', label: t.screenshotTranslationAI },
    { value: 'google', label: t.screenshotTranslationGoogle },
    { value: 'baidu', label: t.screenshotTranslationBaidu },
    { value: 'tencent', label: t.screenshotTranslationTencent },
    { value: 'bing', label: t.screenshotTranslationBing },
    { value: 'bing2', label: t.screenshotTranslationBing2 },
    { value: 'yandex', label: t.screenshotTranslationYandex },
    { value: 'caiyun2', label: t.screenshotTranslationCaiyun2 },
    { value: 'microsoft', label: t.screenshotTranslationMicrosoft },
  ]
  const methodSelectClass = 'ml-auto shrink-0 h-5 max-w-[122px] rounded-md border border-black/[0.06] bg-white/80 px-1.5 text-[10.5px] font-medium text-neutral-500 outline-none hover:text-neutral-700 hover:border-black/[0.12] disabled:opacity-60 dark:border-white/[0.08] dark:bg-neutral-900/70 dark:text-neutral-400 dark:hover:text-neutral-100 dark:hover:border-white/[0.14]'

  const runTranslationNow = useCallback(async (text: string) => {
    const seq = ++translateSeq.current
    const trimmed = text.trim()
    if (!trimmed) {
      setResult('')
      setLoading(false)
      return
    }

    setLoading(true)
    try {
      const translated = await api.translateText(text)
      if (seq !== translateSeq.current) return
      setResult(translated)
    } catch (e) {
      if (seq !== translateSeq.current) return
      console.error(e)
      setResult(typeof e === 'string' ? e : (e as Error).message || 'Error')
    } finally {
      if (seq === translateSeq.current) setLoading(false)
    }
  }, [])

  useEffect(() => {
    let active = true
    api.getSettings()
      .then((settings) => {
        if (!active) return
        setTranslationMethod(settings.screenshotTranslation?.translationMethod || 'ai')
      })
      .catch(err => console.error('[Translator] Failed to load translation method:', err))
    return () => { active = false }
  }, [])

  const handleTranslationMethodChange = useCallback(async (value: string) => {
    const method = value as TranslationMethod
    if (method === translationMethod || methodSwitching) return
    setMethodSwitching(true)
    const previous = translationMethod
    setTranslationMethod(method)
    translateSeq.current += 1
    setResult('')
    setLoading(!!input.trim())
    try {
      const settings = await api.getSettings()
      await api.saveSettings({
        ...settings,
        screenshotTranslation: {
          ...settings.screenshotTranslation,
          translationMethod: method,
        },
      })
      if (input.trim()) {
        await runTranslationNow(input)
      } else {
        setLoading(false)
      }
    } catch (err) {
      setTranslationMethod(previous)
      setLoading(false)
      console.error('[Translator] Failed to switch translation method:', err)
      setResult(err instanceof Error ? err.message : String(err))
    } finally {
      setMethodSwitching(false)
    }
  }, [input, methodSwitching, runTranslationNow, translationMethod])

  // 输入防抖翻译：600ms 延迟后发送翻译请求
  useEffect(() => {
    const trimmed = input.trim()
    if (!trimmed) {
      translateSeq.current += 1
      setResult('')
      setLoading(false)
      return
    }
    if (skipNextDebouncedInputRef.current === input) {
      skipNextDebouncedInputRef.current = null
      return
    }

    const seq = ++translateSeq.current
    const timer = setTimeout(async () => {
      if (seq !== translateSeq.current) return
      setLoading(true)
      try {
        const translated = await api.translateText(input)
        if (seq !== translateSeq.current) return
        setResult(translated)
      } catch (e) {
        if (seq !== translateSeq.current) return
        console.error(e)
        setResult(typeof e === 'string' ? e : (e as Error).message || 'Error')
      } finally {
        if (seq === translateSeq.current) setLoading(false)
      }
    }, 600)
    return () => clearTimeout(timer)
  }, [input])

  const applyPendingTranslatorSelection = useCallback(async () => {
    try {
      const text = await api.takeTranslatorSelection()
      if (!text.trim()) return
      skipNextDebouncedInputRef.current = text
      setInput(text)
      setResult('')
      inputRef.current?.focus({ preventScroll: true })
      void runTranslationNow(text)
    } catch (err) {
      console.error('[Translator] Failed to take selected text:', err)
    }
  }, [runTranslationNow])

  useEffect(() => {
    void applyPendingTranslatorSelection()
    const onFocus = () => { void applyPendingTranslatorSelection() }
    const onHashChange = () => { void applyPendingTranslatorSelection() }
    window.addEventListener('focus', onFocus)
    window.addEventListener('hashchange', onHashChange)
    return () => {
      window.removeEventListener('focus', onFocus)
      window.removeEventListener('hashchange', onHashChange)
    }
  }, [applyPendingTranslatorSelection])

  // Esc 键隐藏窗口
  useEffect(() => {
    const handler = async (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        try {
          await api.closeWindow()
        } catch (err) {
          console.error('[Translator] Failed to hide window:', err)
        }
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [])

  // 结果区域自动滚动到底部
  useEffect(() => {
    if (resultRef.current) {
      resultRef.current.scrollTop = resultRef.current.scrollHeight
    }
  }, [result])

  // 多行原文区中，普通 Enter 保持换行；Ctrl/Command + Enter 才提交翻译结果。
  // IME 合成中（中/日/韩输入法选词按回车）不要触发：isComposing 是组合事件官方标志，
  // keyCode === 229 是浏览器在 IME 拦截 keydown 时的兜底信号，两个条件并查更稳。
  const handleKeyDown = async (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key !== 'Enter') return
    if (e.nativeEvent.isComposing || e.keyCode === 229) return
    if (!e.ctrlKey && !e.metaKey) return
    e.preventDefault()
    const textToCommit = result || input
    if (!textToCommit.trim()) return
    await api.commitTranslation(textToCommit)
    setInput('')
    setResult('')
  }

  return (
    <div className="window-container">
      {/* 卡片：填满外壳 padding 内区域；圆角 + 阴影都在这层 */}
      <div className="window-frosted h-full w-full flex flex-col select-none overflow-hidden relative group">
        {/* 顶部隐形 drag bar */}
        <div
          className="absolute top-0 left-0 right-0 h-8 z-10"
          data-tauri-drag-region
        />

        {/* 设置按钮（悬浮右上角） */}
        <button
          onClick={onOpenSettings}
          className="absolute top-1.5 right-2 z-20 p-1 text-neutral-400 hover:text-neutral-700 dark:text-neutral-500 dark:hover:text-neutral-200 rounded-md hover:bg-black/5 dark:hover:bg-white/10 opacity-60 hover:opacity-100 transition-all duration-150"
          title={t.translatorSettings}
        >
          <SettingsIcon size={13} strokeWidth={1.75} />
        </button>

        {/* 主内容区 */}
        <div className="relative z-0 flex h-full min-h-0 flex-col px-4 pt-4 pb-3">
          <div className="h-5 shrink-0" data-tauri-drag-region />

          <div className="flex min-h-0 flex-1 flex-col gap-3">
            <section className="flex min-h-0 flex-1 flex-col">
              <div className="mb-1.5 flex items-center justify-between">
                <span className="text-[10.5px] font-semibold uppercase tracking-[0.08em] text-neutral-400 dark:text-neutral-500">
                  {t.shotOriginal}
                </span>
              </div>
              <textarea
                ref={inputRef}
                autoFocus
                spellCheck={false}
                className="min-h-0 flex-1 w-full resize-none px-3 py-2.5 bg-white/70 dark:bg-neutral-800/40 ring-1 ring-black/[0.05] dark:ring-white/[0.06] rounded-xl text-[14px] leading-[1.48] text-neutral-900 dark:text-white placeholder-neutral-400 dark:placeholder-neutral-500 focus:outline-none focus:ring-black/[0.12] dark:focus:ring-white/[0.18] focus:bg-white dark:focus:bg-neutral-800/70 transition-all custom-scrollbar select-text"
                placeholder={t.translatorPlaceholder}
                value={input}
                onChange={(e) => setInput(e.target.value)}
                onKeyDown={handleKeyDown}
              />
            </section>

            <section className="flex min-h-0 flex-1 flex-col">
              <div className="mb-1.5 flex items-center gap-1.5">
                <span className="text-[10.5px] font-semibold uppercase tracking-[0.08em] text-neutral-400 dark:text-neutral-500">
                  {t.shotTranslated}
                </span>
                {(loading || methodSwitching) && (
                  <span className="flex items-center gap-1 text-[10.5px] text-neutral-400 dark:text-neutral-500">
                    <Loader2 size={10} className="animate-spin" />
                    {t.translatorTranslating}
                  </span>
                )}
                <select
                  value={translationMethod}
                  disabled={methodSwitching}
                  onChange={(e) => void handleTranslationMethodChange(e.target.value)}
                  title={t.screenshotTranslationMethod}
                  aria-label={t.screenshotTranslationMethod}
                  className={methodSelectClass}
                >
                  {translationMethodOptions.map(option => (
                    <option key={option.value} value={option.value}>{option.label}</option>
                  ))}
                </select>
              </div>
              <div
                ref={resultRef}
                className="min-h-0 flex-1 px-3 py-2.5 rounded-xl overflow-y-auto custom-scrollbar bg-gradient-to-br from-neutral-100/90 to-neutral-50/80 dark:from-neutral-800/70 dark:to-neutral-800/40 ring-1 ring-black/[0.04] dark:ring-white/[0.06] shadow-sm"
              >
                {result ? (
                  <p className="text-neutral-800 dark:text-neutral-100 text-[14px] font-normal select-text leading-[1.55] whitespace-pre-wrap break-words">
                    {result}
                  </p>
                ) : loading ? (
                  <div className="space-y-2 py-0.5">
                    <div className="h-3.5 rounded bg-gradient-to-r from-neutral-200 via-neutral-100 to-neutral-200 dark:from-neutral-800 dark:via-neutral-700 dark:to-neutral-800 bg-[length:200%_100%] animate-[shimmer_1.4s_linear_infinite]" />
                    <div className="h-3.5 rounded bg-gradient-to-r from-neutral-200 via-neutral-100 to-neutral-200 dark:from-neutral-800 dark:via-neutral-700 dark:to-neutral-800 bg-[length:200%_100%] animate-[shimmer_1.4s_linear_infinite] w-[78%]" />
                  </div>
                ) : null}
              </div>
            </section>
          </div>

          {/* 底部提示 */}
          <div className="mt-2.5 flex shrink-0 justify-between items-center text-[10px] text-neutral-400 dark:text-neutral-500">
            <div className="flex items-center gap-2">
              <span>{t.translatorHintEnter}</span>
              <span>{t.translatorHintEsc}</span>
            </div>
            {translateSource && (
              <span className="flex items-center gap-1 opacity-70 max-w-[220px] truncate">
                <Cpu size={9} strokeWidth={1.5} className="shrink-0" />
                <span className="truncate">{translateSource}</span>
              </span>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

/**
 * 应用根组件
 * 根据 URL hash 切换不同视图模式（翻译器、设置、lens）
 */
function App() {
  // 从 URL hash 和查询参数解析当前模式
  const getMode = () => {
    const urlParams = new URLSearchParams(window.location.search)
    const hash = window.location.hash.replace('#', '')
    return urlParams.get('mode') || hash.split('?')[0] || ''
  }

  const [mode, setMode] = useState(getMode)
  const [themeMode, setThemeMode] = useState<'system' | 'light' | 'dark'>('system')
  const [translateSource, setTranslateSource] = useState<string>('')
  const [lang, setLang] = useState<Lang>('zh')

  // 应用主题设置
  const applyTheme = async () => {
    const settings = await api.getSettings()
    const nextMode = (settings.theme || 'system') as 'system' | 'light' | 'dark'
    setThemeMode(nextMode)
    const isDark = nextMode === 'dark' || (nextMode === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches)
    if (isDark) {
      document.documentElement.classList.add('dark')
    } else {
      document.documentElement.classList.remove('dark')
    }
    setTranslateSource(settings.translatorModel || 'AI')
    setLang((settings.settingsLanguage as Lang) || 'zh')
  }

  // 初始化主题并监听系统主题变化
  useEffect(() => {
    applyTheme()
    const mq = window.matchMedia('(prefers-color-scheme: dark)')
    const changeHandler = () => {
      if (themeMode === 'system') applyTheme()
    }
    mq.addEventListener('change', changeHandler)
    return () => mq.removeEventListener('change', changeHandler)
  }, [themeMode])

  // 监听 hash 变化切换模式
  useEffect(() => {
    const handler = () => setMode(getMode())
    window.addEventListener('hashchange', handler)
    return () => window.removeEventListener('hashchange', handler)
  }, [])

  // 监听后端触发的打开设置事件
  // 仅 main webview（hash 为空 / translator / settings）响应；
  // lens webview 即便误收广播也不切换视图，避免多设置界面。
  useEffect(() => {
    let cleanup: (() => void) | undefined
    api.onOpenSettings(() => {
      const currentHash = window.location.hash.replace('#', '').split('?')[0]
      if (currentHash !== '' && currentHash !== 'translator' && currentHash !== 'settings') return
      window.location.hash = '#settings'
      setMode('settings')
    }).then((unlisten) => {
      cleanup = unlisten
    })
    return () => {
      cleanup?.()
    }
  }, [])

  // 根据当前模式调整窗口大小
  useEffect(() => {
    const resize = async () => {
      if (mode === 'settings') {
        await api.resizeWindow(SETTINGS_WINDOW_W, SETTINGS_WINDOW_H)
      } else if (mode === '' || mode === 'translator') {
        await api.resizeWindow(TRANSLATOR_WINDOW_W, TRANSLATOR_WINDOW_H)
      }
    }
    resize()
  }, [mode])

  // 打开设置页
  const openSettings = async () => {
    window.location.hash = '#settings'
    setMode('settings')
    // 确保窗口大小正确，设置页不置顶
    await api.resizeWindow(SETTINGS_WINDOW_W, SETTINGS_WINDOW_H)
    await api.setAlwaysOnTop(false)
  }

  // 关闭设置页，返回翻译器
  const closeSettings = async () => {
    try {
      await api.hideWindow()
    } catch (err) {
      console.error('[App] Error hiding window:', err)
    }
    window.location.hash = ''
    setMode('')
    await api.resizeWindow(TRANSLATOR_WINDOW_W, TRANSLATOR_WINDOW_H)
  }

  // 根据模式渲染对应视图
  if (mode === 'lens') {
    return (
      <Suspense fallback={null}>
        <Lens />
      </Suspense>
    )
  }
  if (mode === 'settings') {
    return (
      <div className="h-screen w-screen overflow-hidden">
        <Suspense fallback={null}>
          <Settings onClose={closeSettings} onSettingsChange={applyTheme} />
        </Suspense>
      </div>
    )
  }
  return <Translator translateSource={translateSource} lang={lang} onOpenSettings={openSettings} />
}

export default App
