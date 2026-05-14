import { AI_LAUNCH_COMMAND_SETTING } from './registry';
import type { AppConfig, PluginSettings, PluginSettingValue } from '@/types/arborist';

export type PluginKind = keyof PluginSettings;

export function pluginEnabled(settings: PluginSettings, kind: PluginKind, pluginId: string, defaultEnabled = true): boolean {
  return settings[kind][pluginId]?.enabled ?? defaultEnabled;
}

export function pluginSetting(settings: PluginSettings, kind: PluginKind, pluginId: string, settingId: string): PluginSettingValue | undefined {
  return settings[kind][pluginId]?.settings[settingId];
}

export function pluginSettingString(settings: PluginSettings, kind: PluginKind, pluginId: string, settingId: string): string | undefined {
  const value = pluginSetting(settings, kind, pluginId, settingId);
  return typeof value === 'string' ? value : undefined;
}

export function aiLaunchCommand(config: AppConfig, pluginId: string): string {
  return pluginSettingString(config.pluginSettings, 'ai', pluginId, AI_LAUNCH_COMMAND_SETTING) ?? config.aiLaunchCommands.commands[pluginId] ?? '';
}
