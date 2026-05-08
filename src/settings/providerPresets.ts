export type ProviderPreset = {
  name: string
  baseUrl: string
  defaultModels: string[]
  onDevice?: boolean
}

export const PROVIDER_PRESETS: ProviderPreset[] = [
  {
    name: 'DeepSeek',
    baseUrl: 'https://api.deepseek.com/v1/chat/completions',
    defaultModels: ['deepseek-chat', 'deepseek-reasoner'],
  },
  {
    name: 'OpenRouter',
    baseUrl: 'https://openrouter.ai/api/v1/chat/completions',
    defaultModels: ['anthropic/claude-sonnet-4.5', 'openai/gpt-4o-mini'],
  },
  {
    name: 'SiliconFlow',
    baseUrl: 'https://api.siliconflow.cn/v1/chat/completions',
    defaultModels: ['Qwen/Qwen2.5-72B-Instruct', 'deepseek-ai/DeepSeek-V3'],
  },
  {
    name: 'GLM',
    baseUrl: 'https://open.bigmodel.cn/api/paas/v4/chat/completions',
    defaultModels: ['glm-4-plus', 'glm-4v-plus'],
  },
  {
    name: 'Ollama',
    baseUrl: 'https://ollama.com/v1/chat/completions',
    defaultModels: ['gpt-oss:120b', 'llama3.3:70b'],
  },
  {
    name: 'Apple Intelligence',
    baseUrl: 'applefoundation://local',
    defaultModels: ['apple-foundation'],
    onDevice: true,
  },
]
