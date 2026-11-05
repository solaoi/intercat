import * as monaco from 'monaco-editor/editor/editor.api';
import EditorWorker from 'monaco-editor/editor/editor.worker?worker';
import 'monaco-editor/features/codicon/register';
import 'monaco-editor/editor/contrib/diffEditorBreadcrumbs/browser/contribution';

export let editorWorkerCount = 0;
self.MonacoEnvironment = {
  getWorker() { editorWorkerCount += 1; return new EditorWorker(); },
};
monaco.editor.defineTheme('intercat', {
  base: 'vs-dark', inherit: true, rules: [],
  colors: { 'editor.background': '#101319', 'editor.lineHighlightBackground': '#181e28', 'editorLineNumber.foreground': '#515e73', 'editorGutter.background': '#101319' },
});
export { monaco };
