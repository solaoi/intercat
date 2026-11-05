import net from 'node:net';

export class RpcClient {
  #socket; #pending = new Map(); #nextId = 0; #buffer = ''; #closed = false;
  constructor(socket) {
    this.#socket = socket;
    socket.setEncoding('utf8');
    socket.on('data', chunk => {
      this.#buffer += chunk;
      if (Buffer.byteLength(this.#buffer) > 8 * 1024 * 1024) return socket.destroy(new Error('RPC response exceeds limit'));
      let end;
      while ((end = this.#buffer.indexOf('\n')) !== -1) {
        const line = this.#buffer.slice(0, end); this.#buffer = this.#buffer.slice(end + 1);
        let response;
        try { response = JSON.parse(line); } catch { socket.destroy(new Error('Invalid RPC response')); return; }
        const pending = this.#pending.get(response.id);
        if (!pending) continue;
        clearTimeout(pending.timer); this.#pending.delete(response.id);
        if (response.error) pending.reject(Object.assign(new Error(response.error.message), { code: response.error.code }));
        else pending.resolve(response.result);
      }
    });
    socket.on('error', () => this.#fail());
    socket.on('close', () => this.#fail());
  }
  #fail() {
    this.#closed = true;
    for (const pending of this.#pending.values()) { clearTimeout(pending.timer); pending.reject(new Error('Daemon connection closed; operation outcome may be unknown')); }
    this.#pending.clear();
  }
  static async connect(path, token) {
    const socket = net.createConnection({ path });
    await new Promise((resolve, reject) => {
      socket.once('connect', resolve); socket.once('error', reject);
    });
    const client = new RpcClient(socket);
    try { await client.request('rpc.authenticate', { token }); return client; }
    catch (error) { client.close(); throw error; }
  }
  request(method, params = {}) {
    if (this.#closed) return Promise.reject(new Error('Daemon is disconnected'));
    const id = ++this.#nextId;
    const frame = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n';
    if (Buffer.byteLength(frame) > 512 * 1024) return Promise.reject(new Error('RPC request exceeds limit'));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.#pending.delete(id); reject(new Error('RPC timed out; operation outcome may be unknown')); }, 15000);
      this.#pending.set(id, { resolve, reject, timer });
      this.#socket.write(frame);
    });
  }
  close() { this.#socket.destroy(); this.#fail(); }
}
