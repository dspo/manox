import { afterEach, describe, expect, it, vi } from 'vitest';

import { copyText } from './copy-on-hover';

describe('copyText', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('writes through the clipboard API when available', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    await copyText('hello');
    expect(writeText).toHaveBeenCalledWith('hello');
  });

  it('falls back to execCommand when the clipboard API rejects', async () => {
    vi.stubGlobal('navigator', {
      clipboard: { writeText: vi.fn().mockRejectedValue(new Error('denied')) },
    });
    const select = vi.fn();
    const execCommand = vi.fn().mockReturnValue(true);
    const body = { appendChild: vi.fn(), removeChild: vi.fn() };
    vi.stubGlobal('document', {
      createElement: () => ({ value: '', style: {}, select }),
      body,
      execCommand,
    });
    await copyText('hello');
    expect(select).toHaveBeenCalled();
    expect(execCommand).toHaveBeenCalledWith('copy');
    expect(body.removeChild).toHaveBeenCalled();
  });
});
