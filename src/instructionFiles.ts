export function isInstructionFile(path: string): boolean {
  const parts = path.split('/');
  if (parts.some(part => !part || part === '.' || part === '..' || part === '.git' || part === '.intercat')) return false;
  const name = parts.at(-1), parent = parts.at(-2);
  if (parent === '.agents') return name === 'AGENTS.md';
  if (parent === '.claude') return name === 'CLAUDE.md';
  return name === 'AGENTS.md' || name === 'CLAUDE.md';
}
