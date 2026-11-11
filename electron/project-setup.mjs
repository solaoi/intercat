import path from 'node:path';
import { synchronizeProjectSecrets } from './project-secrets.mjs';

// Paths originate in the OS picker. A renderer can resolve its own choice by
// ID, but cannot replace the selected paths or choose a different root.
export class ProjectSetup {
  #pending = new Map();
  #completed = new Map();
  constructor(connection, operationId) { this.connection = connection; this.operationId = operationId; }
  current(owner) {
    const selection = this.#pending.get(owner);
    return selection ? this.#result(selection) : null;
  }
  #result(selection) {
    return { project: null, selection: { id: selection.id, issue: selection.issue, error: selection.error, mayHaveSaved: selection.mayHaveSaved } };
  }
  async begin(owner, paths) {
    if (this.#pending.has(owner)) return this.current(owner);
    this.#completed.delete(owner);
    const selection = { rootChosen: false, id: this.operationId(), paths: [...paths], issue: null, error: null, mayHaveSaved: false };
    this.#pending.set(owner, selection);
    try {
      selection.issue = await (await this.connection()).request('project.selection', { paths: selection.paths });
      if (this.#pending.get(owner) !== selection) return { project: null };
      if (selection.issue) return this.#result(selection);
    } catch (error) {
      selection.error = error.message;
      return this.#result(selection);
    }
    return this.#configure(owner, selection);
  }
  async resolve(owner, request) {
    if (!request || Object.keys(request).sort().join(',') !== 'action,id' || !['useRoot', 'retry', 'cancel'].includes(request.action)) throw new Error('Invalid folder choice');
    const selection = this.#pending.get(owner);
    if (!selection || request.id !== selection.id) {
      const completed = this.#completed.get(owner);
      if (completed?.id === request.id) return completed.result;
      throw new Error('This folder choice is no longer available.');
    }
    if (request.action === 'cancel') {
      if (selection.mayHaveSaved) throw new Error('Retry saving this project choice before closing it.');
      const result = { project: null };
      this.#completed.set(owner, { id: selection.id, result });
      this.#pending.delete(owner); return result;
    }
    if (request.action === 'useRoot') {
      if (['duplicate', 'repositoryRoot'].includes(selection.issue?.kind)) {
        selection.paths = [...selection.issue.roots];
        selection.issue = null; selection.rootChosen = true;
      } else if (!selection.rootChosen || selection.issue) throw new Error('This choice has no repository root to use.');
    } else if (selection.issue) throw new Error('Resolve this folder issue before retrying.');
    if (!selection.mayHaveSaved) {
      try {
        selection.issue = await (await this.connection()).request('project.selection', { paths: selection.paths });
        if (this.#pending.get(owner) !== selection) return { project: null };
        if (selection.issue) { selection.error = null; return this.#result(selection); }
      } catch (error) { selection.error = error.message; return this.#result(selection); }
    }
    return this.#configure(owner, selection);
  }
  async #configure(owner, selection) {
    if (this.#pending.get(owner) !== selection) return { project: null };
    selection.error = null;
    // After dispatch, retain the same operation and paths even if its reply is
    // lost. Reinspection must not replace an outcome that may already be saved.
    selection.mayHaveSaved = true;
    let acknowledged = false;
    try {
      const client = await this.connection();
      const project = await client.request('project.configure', { opId: selection.id, name: path.basename(selection.paths[0]), paths: selection.paths });
      acknowledged = true;
      await synchronizeProjectSecrets(client, this.operationId());
      const result = { project };
      if (this.#pending.get(owner) === selection) {
        this.#completed.set(owner, { id: selection.id, result });
        this.#pending.delete(owner);
      }
      return result;
    } catch (error) {
      if (!acknowledged && Number.isInteger(error.code)) selection.mayHaveSaved = false;
      selection.error = error.message; return this.#result(selection);
    }
  }
  forget(owner) { this.#pending.delete(owner); this.#completed.delete(owner); }
}
