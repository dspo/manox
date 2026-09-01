import { describe, expect, it } from 'vitest';

import { formatCost, formatTokens, formatTokensPi } from './usage-format';

describe('formatTokens', () => {
  it('uses lowercase units with one decimal', () => {
    expect(formatTokens(999)).toBe('999');
    expect(formatTokens(1_000)).toBe('1.0k');
    expect(formatTokens(104_200)).toBe('104.2k');
    expect(formatTokens(1_000_000)).toBe('1.0m');
    expect(formatTokens(15_000_000)).toBe('15.0m');
  });
});

describe('formatTokensPi', () => {
  it('uses uppercase M and goes integral above 10M', () => {
    expect(formatTokensPi(999)).toBe('999');
    expect(formatTokensPi(1_000)).toBe('1.0k');
    expect(formatTokensPi(1_000_000)).toBe('1.0M');
    expect(formatTokensPi(9_900_000)).toBe('9.9M');
    expect(formatTokensPi(14_000_000)).toBe('14M');
    expect(formatTokensPi(451_000_000)).toBe('451M');
  });
});

describe('formatCost', () => {
  it('picks decimals by magnitude tier', () => {
    expect(formatCost(1.234)).toBe('$1.23');
    expect(formatCost(0.0234)).toBe('$0.023');
    expect(formatCost(0.00012)).toBe('$0.0001');
  });
});
