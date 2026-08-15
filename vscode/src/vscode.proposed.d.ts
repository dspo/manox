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
}
