// Braille-dot spinner: cycles through the classic 10-frame set to indicate
// in-progress state, mirroring the gpui host's animated braille text.

import { useEffect, useState } from 'react';

import { cn } from '../../lib/utils';

const SPINNER_FRAMES = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

export const BrailleSpinner = ({ className }: { className?: string }) => {
  const [frame, setFrame] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => setFrame((f) => (f + 1) % SPINNER_FRAMES.length), 80);
    return () => clearInterval(timer);
  }, []);
  return <span className={cn('inline-block leading-none', className)}>{SPINNER_FRAMES[frame]}</span>;
};
