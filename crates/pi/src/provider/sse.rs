// SSE (Server-Sent Events) frame parser.
//
// Incrementally decodes a byte stream into `data:` payloads. Shared by every
// SSE-based provider (Anthropic, OpenAI, ...). Filled in during the SSE phase.
