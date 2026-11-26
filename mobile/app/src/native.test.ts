import { beforeEach, expect, it, vi } from 'vitest';
import { appVersionLabel } from './native';

const { isNativePlatform, appInfo } = vi.hoisted(() => ({
  isNativePlatform: vi.fn(),
  appInfo: vi.fn(),
}));
vi.mock('@capacitor/core', () => ({
  Capacitor: { isNativePlatform },
  registerPlugin: () => ({ appInfo }),
}));

beforeEach(() => {
  isNativePlatform.mockReset().mockReturnValue(true);
  appInfo.mockReset();
});

it('shows the installed native app version and build without enrollment', async () => {
  appInfo.mockResolvedValue({ version: '2.3.1', build: '42' });
  expect(await appVersionLabel()).toBe('Version 2.3.1 · Build 42');
});

it('identifies a browser preview without calling native code', async () => {
  isNativePlatform.mockReturnValue(false);
  expect(await appVersionLabel()).toBe('Web preview');
  expect(appInfo).not.toHaveBeenCalled();
});

it('shows an unavailable label for missing metadata or a bridge error', async () => {
  appInfo.mockResolvedValueOnce({ version: '', build: '42' });
  appInfo.mockRejectedValueOnce(new Error('bridge failed'));
  expect(await appVersionLabel()).toBe('Version unavailable');
  expect(await appVersionLabel()).toBe('Version unavailable');
});
