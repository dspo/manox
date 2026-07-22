# Prompt token budget

The hard gate for the static system prompt plus Default-mode instructions is
1,200 DeepSeek V3 tokens per language. Run the exact tokenizer check with:

```sh
python3 scripts/check_prompt_tokens.py /path/to/deepseek_v3_tokenizer.zip
```

The script reads `tokenizer.json` directly from the zip and exits non-zero if
either language exceeds the budget. With the supplied DeepSeek V3 tokenizer,
the current results are:

| Language | System | Default mode | Combined | Budget |
|---|---:|---:|---:|---:|
| English | 411 | 125 | 536 | 1,200 |
| Chinese | 439 | 115 | 554 | 1,200 |

Against the measured pre-optimization baselines (3,846 English and 3,524
Chinese tokens), the combined prompts are now 13.9% and 15.7% respectively.
Both are therefore comfortably below the original 25% target as well as the
issue's absolute 1,200-token gate.

The combined count tokenizes the actual concatenation, so it is intentionally
not assumed to equal the sum of the two independently tokenized files.
