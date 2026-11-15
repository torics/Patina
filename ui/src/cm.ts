// CodeMirror for the editor and the diff view. Loaded on demand, so the app's first load stays small; each
// language is its own chunk too.
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { bracketMatching, foldGutter, HighlightStyle, indentOnInput, syntaxHighlighting, type LanguageSupport } from "@codemirror/language";
import { unifiedMergeView } from "@codemirror/merge";
import { highlightSelectionMatches, search, searchKeymap } from "@codemirror/search";
import { Compartment, EditorState, type Extension } from "@codemirror/state";
import { drawSelection, EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers } from "@codemirror/view";
import { tags as t } from "@lezer/highlight";

/** Colors come from the page's custom properties, so the editor follows the light and dark themes. */
const theme = EditorView.theme({
  "&": { color: "var(--text)", backgroundColor: "var(--bg)", fontSize: "13px", height: "100%" },
  ".cm-scroller": { fontFamily: "ui-monospace, monospace", lineHeight: "1.45" },
  ".cm-content": { caretColor: "var(--accent)" },
  ".cm-gutters": { backgroundColor: "var(--panel)", color: "var(--muted)", borderRight: "1px solid var(--line)" },
  ".cm-activeLine, .cm-activeLineGutter": { backgroundColor: "color-mix(in srgb, var(--accent) 8%, transparent)" },
  "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": { backgroundColor: "color-mix(in srgb, var(--accent) 30%, transparent)" },
  "&.cm-focused": { outline: "2px solid var(--accent)", outlineOffset: "-2px" },
  ".cm-panels": { backgroundColor: "var(--panel)", color: "var(--text)" },
  ".cm-searchMatch": { backgroundColor: "color-mix(in srgb, var(--warn) 35%, transparent)" },
  ".cm-changedLine": { backgroundColor: "color-mix(in srgb, var(--accent) 12%, transparent)" },
  ".cm-deletedChunk": { backgroundColor: "color-mix(in srgb, var(--danger) 14%, transparent)" },
  ".cm-insertedLine": { backgroundColor: "color-mix(in srgb, var(--accent) 14%, transparent)" },
  ".cm-changedText": { background: "color-mix(in srgb, var(--accent) 35%, transparent)" },
  ".cm-deletedText": { background: "color-mix(in srgb, var(--danger) 35%, transparent)" },
});

const highlight = HighlightStyle.define([
  { tag: [t.keyword, t.controlKeyword, t.moduleKeyword, t.operatorKeyword], color: "var(--syntax-keyword)" },
  { tag: [t.string, t.special(t.string), t.regexp], color: "var(--syntax-string)" },
  { tag: [t.number, t.bool, t.null, t.atom], color: "var(--syntax-number)" },
  { tag: [t.comment, t.lineComment, t.blockComment], color: "var(--muted)", fontStyle: "italic" },
  { tag: [t.function(t.variableName), t.function(t.propertyName)], color: "var(--syntax-function)" },
  { tag: [t.typeName, t.className, t.namespace], color: "var(--syntax-type)" },
  { tag: [t.heading], color: "var(--syntax-keyword)", fontWeight: "bold" },
  { tag: [t.link, t.url], color: "var(--accent)", textDecoration: "underline" },
  { tag: [t.emphasis], fontStyle: "italic" },
  { tag: [t.strong], fontWeight: "bold" },
]);

/** The language support for a file name, loaded on first use. */
async function language(path: string): Promise<LanguageSupport | null> {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  switch (ext) {
    case "js":
    case "mjs":
    case "cjs":
    case "jsx":
      return (await import("@codemirror/lang-javascript")).javascript({ jsx: true });
    case "ts":
    case "mts":
    case "cts":
    case "tsx":
      return (await import("@codemirror/lang-javascript")).javascript({ jsx: ext === "tsx", typescript: true });
    case "json":
      return (await import("@codemirror/lang-json")).json();
    case "rs":
      return (await import("@codemirror/lang-rust")).rust();
    case "py":
      return (await import("@codemirror/lang-python")).python();
    case "md":
    case "markdown":
      return (await import("@codemirror/lang-markdown")).markdown();
    case "css":
      return (await import("@codemirror/lang-css")).css();
    case "html":
    case "htm":
      return (await import("@codemirror/lang-html")).html();
    default:
      return null;
  }
}

/** A place for an editor inside a shadow root of `host`. CodeMirror then styles itself with constructed style
 * sheets, which the page's Content Security Policy allows, instead of a <style> element, which it blocks. The
 * page's custom properties still reach it, so the theme follows. */
function mountPoint(host: HTMLElement): { root: ShadowRoot; parent: HTMLElement } {
  const root = host.shadowRoot ?? host.attachShadow({ mode: "open" });
  const parent = document.createElement("div");
  parent.style.height = "100%";
  root.replaceChildren(parent);
  return { root, parent };
}

function base(path: string): Extension[] {
  return [
    lineNumbers(),
    foldGutter(),
    drawSelection(),
    bracketMatching(),
    highlightSelectionMatches(),
    syntaxHighlighting(highlight),
    theme,
    EditorView.contentAttributes.of({ "aria-label": `Content of ${path}` }),
  ];
}

export type EditorHandle = { text: () => string; setReadOnly: (ro: boolean) => void; destroy: () => void };

/** An editor for one file. `onChange` hears every edit; Ctrl+S or Cmd+S calls `onSave`. */
export async function mountEditor(host: HTMLElement, path: string, text: string, readOnly: boolean, onChange: (text: string) => void, onSave: () => void): Promise<EditorHandle> {
  const lang = await language(path);
  const editable = new Compartment();
  const ro = (on: boolean) => [EditorState.readOnly.of(on), EditorView.editable.of(!on)];
  const { root, parent } = mountPoint(host);
  const view = new EditorView({
    parent,
    root,
    state: EditorState.create({
      doc: text,
      extensions: [
        ...base(path),
        history(),
        indentOnInput(),
        highlightActiveLine(),
        highlightActiveLineGutter(),
        search({ top: true }),
        keymap.of([{ key: "Mod-s", preventDefault: true, run: () => (onSave(), true) }, indentWithTab, ...defaultKeymap, ...historyKeymap, ...searchKeymap]),
        editable.of(ro(readOnly)),
        EditorView.updateListener.of((u) => u.docChanged && onChange(u.state.doc.toString())),
        lang ?? [],
      ],
    }),
  });
  return {
    text: () => view.state.doc.toString(),
    setReadOnly: (on) => view.dispatch({ effects: editable.reconfigure(ro(on)) }),
    destroy: () => (view.destroy(), parent.remove()),
  };
}

/** A read-only diff of one file: the run's version, with what it replaced shown inline. */
export async function mountDiff(host: HTMLElement, path: string, before: string, after: string): Promise<{ destroy: () => void }> {
  const lang = await language(path);
  const { root, parent } = mountPoint(host);
  const view = new EditorView({
    parent,
    root,
    state: EditorState.create({
      doc: after,
      extensions: [
        ...base(path),
        EditorState.readOnly.of(true),
        EditorView.editable.of(false),
        unifiedMergeView({ original: before, mergeControls: false, highlightChanges: true, gutter: true, collapseUnchanged: { margin: 3, minSize: 6 } }),
        lang ?? [],
      ],
    }),
  });
  return { destroy: () => (view.destroy(), parent.remove()) };
}
