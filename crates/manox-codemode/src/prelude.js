"use strict";
// Environment contract evaluated inside the QuickJS VM before every script.
//
// The prelude evaluates to a factory: `(hostCall, hostOutput, hostDone,
// toolsJson, storeJson) => ({ run, settle, pendingCount })`. The host bridge
// enters only through the factory parameters, so the script cannot reach the
// natives: they are never globals.
//
// Script-visible surface (nothing else exists — no timers, fetch, fs, process,
// require, or modules):
// - tools.<jsName>(args): the only capability that leaves the VM. Every call
//   returns a promise; results cross back as JSON strings via `settle`.
//   `tools` also carries the original tool names, so `tools["mcp__a-b"]`
//   works. Reading an unknown member yields a function whose call rejects.
// - text(v) / console.log|info|warn|error|debug: append text items (console
//   items are rendered as one block after the body by the host).
// - image(v): append an image from a base64 data: URL, `{image_url}`, or an
//   image block { type: "image", data, mimeType }. Remote http(s) URLs are
//   rejected; PNG/JPEG/GIF/WebP payloads are detected by signature.
// - return v / exit(): `return` serializes v as JSON (the host reports it as
//   the value); `exit()` ends the execution successfully right away.
// - store(key, value) / load(key): synchronous JSON store over the snapshot
//   passed in as storeJson. store(key, undefined) deletes. Violating the
//   limits throws a RangeError inside the VM.
// - ALL_TOOLS: frozen [{ name, description }] for every tool.
// - describeTool(name): description plus the rendered TypeScript declaration,
//   or undefined for an unknown name.
//
// Limits (violations end the execution with a script error):
// - output: 16 Mi characters / 100000 items (text and image data combined)
// - store: 256 Ki per value / 1 Mi total, counted as JSON text characters
(function (hostCall, hostOutput, hostDone, toolsJson, storeJson) {
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const pending = new Map();
  let nextId = 1;
  let finished = false;
  const MAX_STORE_VALUE_CHARS = 262144; // 256 Ki
  const MAX_STORE_TOTAL_CHARS = 1048576; // 1 Mi
  const MAX_OUTPUT_CHARS = 16777216; // 16 Mi
  const MAX_OUTPUT_ITEMS = 100000;
  const IMAGE_EXPECTS =
    'image expects a base64 data: URL string, an object with image_url, or an image block { type: "image", data, mimeType }';
  // Thrown by exit() to unwind the script after success was already reported.
  const EXIT = Object.freeze({});

  function done(ok, payload, writesJson) {
    if (finished) return;
    finished = true;
    hostDone(ok, payload === undefined ? null : payload, writesJson === undefined ? null : writesJson);
  }

  // QuickJS stacks list frames only; prefix "Name: message" so the text reads
  // like a Node error, and drop this prelude's frames.
  function errorText(error) {
    const head = error.message ? error.name + ": " + error.message : String(error.name);
    const frames =
      typeof error.stack === "string"
        ? error.stack
            .split("\n")
            .filter((line) => line.trim() && line.indexOf("manox-codemode-prelude.js") === -1)
        : [];
    return [head].concat(frames).join("\n");
  }

  function format(value) {
    if (typeof value === "string") return value;
    if (value instanceof Error) return errorText(value);
    if (typeof value === "number") return String(value);
    if (value === undefined || value === null || typeof value === "boolean") return String(value);
    try {
      const json = stringify(value);
      return json === undefined ? String(value) : json;
    } catch (error) {
      return String(value);
    }
  }

  // A script can set an error's name or message to anything, so both are
  // coerced; a value whose coercion throws gets a placeholder.
  function describeError(error) {
    try {
      if (error instanceof Error) {
        return stringify({ name: String(error.name), message: String(error.message), stack: errorText(error) });
      }
      return stringify({ message: format(error) });
    } catch (error) {
      return stringify({ message: "The script threw a value that cannot be described" });
    }
  }

  function caller(name) {
    return function () {
      const args = arguments;
      return new Promise(function (resolve, reject) {
        let json;
        try {
          json =
            args.length === 0
              ? "null"
              : stringify(args.length === 1 ? args[0] : Array.prototype.slice.call(args));
        } catch (error) {
          reject(error);
          return;
        }
        const id = nextId++;
        pending.set(id, { resolve: resolve, reject: reject });
        hostCall(id, name, json);
      });
    };
  }

  const tools = Object.create(null);
  const allTools = [];
  const described = new Map();
  for (const def of parse(toolsJson)) {
    const fn = caller(def.name);
    // The first tool wins when two names normalize to the same identifier.
    if (!(def.jsName in tools)) {
      tools[def.jsName] = fn;
      allTools.push(Object.freeze({ name: def.jsName, description: def.description }));
    }
    if (!(def.name in tools)) tools[def.name] = fn;
    if (!described.has(def.name)) described.set(def.name, def);
    if (!described.has(def.jsName)) described.set(def.jsName, def);
  }
  Object.freeze(allTools);

  function describeTool(name) {
    const def = typeof name === "string" ? described.get(name) : undefined;
    if (def === undefined) return undefined;
    const head = def.description;
    if (typeof def.declaration !== "string") return head;
    return (
      head +
      "\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { " +
      def.declaration +
      " };\n```"
    );
  }

  // Reading an unknown member returns a function whose call rejects with a
  // TypeError, so a typo rejects the awaiting continuation instead of throwing
  // synchronously at the call site.
  const toolNames = allTools.map((tool) => tool.name);
  const toolsProxy = new Proxy(tools, {
    get(object, property, receiver) {
      if (
        typeof property !== "string" ||
        property in object ||
        property in Object.prototype ||
        property === "then" ||
        property === "toJSON"
      ) {
        return Reflect.get(object, property, receiver);
      }
      const available = toolNames.length <= 20 ? " Available: " + toolNames.join(", ") + "." : "";
      return function () {
        return Promise.reject(
          new TypeError(
            "tools." + property + " is not an available tool." + available + " ALL_TOOLS lists every available tool.",
          ),
        );
      };
    },
  });

  // key -> JSON text. Sizes count key and JSON characters.
  const stored = new Map(Object.entries(parse(storeJson)));
  const writes = new Map();
  let storedChars = 0;
  for (const [key, json] of stored) storedChars += key.length + json.length;

  function store(key, value) {
    if (typeof key !== "string") throw new TypeError("store() key must be a string");
    const previous = stored.has(key) ? key.length + stored.get(key).length : 0;
    if (value === undefined) {
      stored.delete(key);
      storedChars -= previous;
      writes.set(key, undefined);
      return;
    }
    let json;
    try {
      json = stringify(value);
    } catch (error) {
      throw new TypeError("store(" + stringify(key) + ") value is not JSON-serializable: " + format(error));
    }
    if (json === undefined) {
      throw new TypeError("store(" + stringify(key) + ") value is not JSON-serializable");
    }
    if (json.length > MAX_STORE_VALUE_CHARS) {
      throw new RangeError(
        "store(" +
          stringify(key) +
          ") value is " +
          json.length +
          " characters of JSON, over the limit of " +
          MAX_STORE_VALUE_CHARS +
          ". store() is for small state such as IDs or summaries; keep large data in variables or write it to a file with a tool.",
      );
    }
    const next = storedChars - previous + key.length + json.length;
    if (next > MAX_STORE_TOTAL_CHARS) {
      throw new RangeError(
        "store is full: stored values would exceed " +
          MAX_STORE_TOTAL_CHARS +
          " characters of JSON. Delete keys with store(key, undefined).",
      );
    }
    stored.set(key, json);
    storedChars = next;
    writes.set(key, json);
  }

  function load(key) {
    if (typeof key !== "string") throw new TypeError("load() key must be a string");
    const json = stored.get(key);
    return json === undefined ? undefined : parse(json);
  }

  function serializeWrites() {
    const entries = [];
    for (const [key, json] of writes) entries.push(json === undefined ? [key] : [key, json]);
    return stringify(entries);
  }

  let outputChars = 0;
  let outputItems = 0;

  // Past the output limits the script fails: done() already reported the
  // error, so catching it does not resume output.
  function output(kind, data, mimeType) {
    if (finished) return;
    outputChars += data.length;
    outputItems++;
    if (outputChars > MAX_OUTPUT_CHARS || outputItems > MAX_OUTPUT_ITEMS) {
      const error = new RangeError(
        "script output exceeded the limit of " +
          MAX_OUTPUT_CHARS +
          " characters or " +
          MAX_OUTPUT_ITEMS +
          " text(), image(), and console calls. Print a summary instead, or write large data to a file with a tool.",
      );
      done(false, describeError(error), undefined);
      throw error;
    }
    hostOutput(kind, data, mimeType === undefined ? null : mimeType);
  }

  // Primitives become their string form, everything else JSON.
  function outputText(value) {
    if (value === undefined || value === null || (typeof value !== "object" && typeof value !== "function")) {
      return String(value);
    }
    const json = stringify(value);
    return json === undefined ? String(value) : json;
  }

  function text(value) {
    let rendered;
    try {
      rendered = outputText(value);
    } catch (error) {
      throw new TypeError(error instanceof Error ? error.message : String(error));
    }
    output("text", rendered);
  }

  // Base64 of the signatures of the formats providers accept inline (PNG,
  // JPEG except JPEG-LS, GIF, "RIFF....WEBP"). Signatures start at byte 0, so
  // their encodings are prefixes of the data.
  const IMAGE_SIGNATURES = [
    ["image/png", /^iVBORw0KGg/],
    ["image/jpeg", /^\/9j\/(?!9)/],
    ["image/gif", /^R0lGOD[dl]h/],
    ["image/webp", /^UklG.{8}RUJQ/],
  ];

  function imageDataUrl(value) {
    if (typeof value === "string") return value;
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
      throw new TypeError(IMAGE_EXPECTS);
    }
    if (value.image_url !== undefined) {
      if (typeof value.image_url !== "string") throw new TypeError(IMAGE_EXPECTS);
      return value.image_url;
    }
    if (typeof value.type !== "string") throw new TypeError(IMAGE_EXPECTS);
    if (value.type !== "image") {
      throw new TypeError('image only accepts image blocks, got "' + value.type + '"');
    }
    if (typeof value.data !== "string" || value.data === "") {
      throw new TypeError("image expected image block data");
    }
    if (value.data.toLowerCase().startsWith("data:")) return value.data;
    return "data:;base64," + value.data;
  }

  function image(value) {
    const url = imageDataUrl(value);
    if (url === "") throw new TypeError(IMAGE_EXPECTS);
    const colon = url.indexOf(":");
    const scheme = colon === -1 ? "" : url.slice(0, colon).toLowerCase();
    if (scheme === "http" || scheme === "https") {
      throw new TypeError("remote image URLs are not supported in tool outputs. Pass a base64 data URI instead");
    }
    const comma = url.indexOf(",");
    const header = comma === -1 ? [] : url.slice(colon + 1, comma).split(";");
    if (scheme !== "data" || comma === -1 || !header.slice(1).some((part) => part.toLowerCase() === "base64")) {
      throw new TypeError("invalid image output. Pass a base64 data URI instead");
    }
    // Providers reject the whole request on a bad image, so validate: drop
    // line breaks from wrapped base64, then check shape and signature. The
    // declared type is ignored in favor of the detected one.
    const data = url.slice(comma + 1).replace(/\s+/g, "");
    if (data.length % 4 !== 0 || !/^[A-Za-z0-9+/]+={0,2}$/.test(data)) {
      throw new TypeError("invalid image output. The image data is not valid base64 (truncated or corrupted?)");
    }
    const signature = IMAGE_SIGNATURES.find(([, pattern]) => pattern.test(data.slice(0, 16)));
    if (!signature) {
      throw new TypeError("invalid image output. The image data is not a PNG, JPEG, GIF, or WebP image");
    }
    output("image", data, signature[0]);
  }

  function exit() {
    let writesJson;
    try {
      writesJson = serializeWrites();
    } catch (error) {
      done(false, describeError(error), undefined);
      throw EXIT;
    }
    done(true, undefined, writesJson);
    throw EXIT;
  }

  const console = {};
  for (const level of ["log", "info", "warn", "error", "debug"]) {
    console[level] = function () {
      const parts = [];
      for (let i = 0; i < arguments.length; i++) parts.push(format(arguments[i]));
      output("console", parts.join(" "));
    };
  }
  Object.freeze(console);

  Object.defineProperty(globalThis, "tools", { value: toolsProxy, enumerable: true });
  Object.defineProperty(globalThis, "ALL_TOOLS", { value: allTools, enumerable: true });
  Object.defineProperty(globalThis, "console", { value: console, enumerable: true });
  Object.defineProperty(globalThis, "text", { value: text, enumerable: true });
  Object.defineProperty(globalThis, "image", { value: image, enumerable: true });
  Object.defineProperty(globalThis, "exit", { value: exit, enumerable: true });
  Object.defineProperty(globalThis, "store", { value: store, enumerable: true });
  Object.defineProperty(globalThis, "load", { value: load, enumerable: true });
  Object.defineProperty(globalThis, "describeTool", { value: describeTool, enumerable: true });

  function run(fn) {
    let promise;
    try {
      promise = fn(toolsProxy, console);
    } catch (error) {
      done(false, describeError(error), undefined);
      return;
    }
    Promise.prototype.then.call(
      promise,
      function (value) {
        let json;
        try {
          json = value === undefined ? undefined : stringify(value);
        } catch (error) {
          done(false, describeError(error), undefined);
          return;
        }
        let writesJson;
        try {
          writesJson = serializeWrites();
        } catch (error) {
          done(false, describeError(error), undefined);
          return;
        }
        done(true, json, writesJson);
      },
      function (error) {
        done(false, describeError(error), undefined);
      },
    );
  }

  // Results arrive out of order; the call id pairs them back to resolvers.
  function settle(id, ok, payload) {
    const entry = pending.get(id);
    if (!entry) return;
    pending.delete(id);
    if (!ok) {
      entry.reject(new Error(payload));
      return;
    }
    let value;
    try {
      value = parse(payload);
    } catch (error) {
      entry.reject(error);
      return;
    }
    entry.resolve(value);
  }

  // The host fails the script when the job queue is empty, the script has not
  // finished, and nothing is pending: with no timers or IO in the VM, nothing
  // can ever resume it.
  function pendingCount() {
    return pending.size;
  }

  return { run: run, settle: settle, pendingCount: pendingCount };
})
