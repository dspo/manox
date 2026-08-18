// Typings for the proposed APIs this extension enables via
// `enabledApiProposals` in package.json. The extension host injects these
// symbols into the `vscode` module at runtime only for builds that declare
// the proposals; the declarations cover exactly the surface this extension
// uses and stay out of the stable `LanguageModelResponsePart` union.

declare module 'vscode' {
  export class LanguageModelThinkingPart {
    value: string | string[];
    id?: string;
    metadata?: { readonly [key: string]: any };
    constructor(
      value: string | string[],
      id?: string,
      metadata?: { readonly [key: string]: any },
    );
  }

  export interface LanguageModelChatInformation {
    /**
     * Whether the model shows up in the model picker immediately once the
     * provider makes it known.
     */
    readonly isUserSelectable?: boolean;
  }

  // Mirrors the chatProvider proposal's options: @types/vscode does not ship
  // the proposed typings, so this local declaration supplies `configuration`.
  export interface PrepareLanguageModelChatModelOptions {
    /**
     * Per-group configuration resolved from the language models config file
     * (the group's own properties, minus reserved keys). Present only on the
     * per-group resolution calls, absent on the initial groupless listing.
     */
    readonly configuration?: { readonly [key: string]: any };
  }
}
