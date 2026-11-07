// Draft persistence belongs to the daemon's SQLite writer. The main process
// keeps outstanding writes alive when their originating renderer closes.
export function draftStore(client) {
  const pending = new Set();
  const failures = new Map();
  const failureKey = (session, owner) => `${session}:${owner}`;
  async function read(session) {
    await Promise.allSettled([...pending]);
    return (await client()).request('draft.read', { session });
  }
  function write(request, owner) {
    const key = failureKey(request.session, owner);
    const task = client().then(connection => connection.request('draft.save', request)).then(result => {
      if (result.status === 'saved') failures.delete(key);
      else failures.set(key, new Error('A draft changed in another window. Resolve it before closing.'));
      return result;
    }).catch(error => { failures.set(key, error); throw error; });
    pending.add(task);
    void task.finally(() => pending.delete(task)).catch(() => {});
    return task;
  }
  async function flush() {
    while (pending.size) await Promise.all([...pending]);
    if (failures.size) throw [...failures.values()][0];
  }
  async function discard(session, owner) {
    const saved = await read(session);
    failures.delete(failureKey(session, owner));
    return saved;
  }
  return { read, write, flush, discard };
}
