import type { TokenUsageSnapshot } from '../../../../protocol';

function formatK(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}

export type UsageBarProps = {
  usage: TokenUsageSnapshot | null;
};

export const UsageBar = ({ usage }: UsageBarProps) => {
  if (!usage) {
    return null;
  }
  const input =
    (usage.input_tokens ?? 0) +
    (usage.cache_creation_input_tokens ?? 0) +
    (usage.cache_read_input_tokens ?? 0);
  const output = usage.output_tokens ?? 0;
  if (!input && !output) {
    return null;
  }
  return (
    <div className="font-chrome flex items-center justify-end gap-2 border-t px-3 py-1 text-muted-foreground text-xs">
      <span>in {formatK(input)}</span>
      <span>out {formatK(output)}</span>
    </div>
  );
};
