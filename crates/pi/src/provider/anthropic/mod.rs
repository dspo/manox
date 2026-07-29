// Anthropic Messages API provider.
//
// `wire` mirrors the API schema field-for-field; `translate` converts between
// the domain types and the wire types; `AnthropicStreamFn` (added in the
// streaming phase) implements `StreamFn` on top of both.

pub mod translate;
pub mod wire;
