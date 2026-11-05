import { Children, isValidElement, useEffect, useRef, useState, type ReactNode } from 'react';
import Markdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { createPortal } from 'react-dom';
import './markdown.css';

type Kind = 'image' | 'link';
type Review = Awaited<ReturnType<Window['intercat']['reviewExternal']>>;
function Confirmation({ review, cancel, confirm }: { review: Review; cancel: () => void; confirm: () => void }) {
  const element = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = element.current!;
    dialog.showModal();
    return () => { if (dialog.open) dialog.close(); };
  }, []);
  return createPortal(<dialog ref={element} className="external-confirmation" aria-label={review.kind === 'image' ? 'Confirm image URL' : 'Confirm link URL'} onKeyDown={event => { if (event.key === 'Escape') event.stopPropagation(); }} onCancel={event => { event.preventDefault(); cancel(); }}>
    <p>{review.kind === 'image' ? 'Load this image once?' : 'Open this link in your default application?'}</p>
    <code tabIndex={0} aria-label="Destination URL">{review.url}</code>
    <div className="external-actions"><button autoFocus onClick={cancel}>Cancel</button><button onClick={confirm}>{review.kind === 'image' ? 'Load image' : 'Open link'}</button></div>
  </dialog>, document.body);
}
function destination(url: string, kind: Kind): URL | null {
  try {
    if (/[\u0000-\u001f\u007f]/.test(url)) return null;
    const value = new URL(url);
    return !value.username && !value.password && (['http:', 'https:'].includes(value.protocol) || kind === 'link' && value.protocol === 'mailto:') ? value : null;
  } catch { return null; }
}
function textLabel(children: ReactNode): string {
  return Children.toArray(children).map(child => typeof child === 'string' || typeof child === 'number' ? String(child)
    : isValidElement<{ alt?: string; children?: ReactNode }>(child) ? child.props.alt ?? textLabel(child.props.children) : '').join('');
}
function containsImage(children: ReactNode): boolean {
  return Children.toArray(children).some(child => isValidElement<{ alt?: string; children?: ReactNode }>(child)
    && (child.props.alt !== undefined || containsImage(child.props.children)));
}

function ExternalTarget({ url, kind, label }: { url: string; kind: Kind; label: string }) {
  const [review, setReview] = useState<Review | null>(null);
  const [image, setImage] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const working = useRef(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const container = useRef<HTMLSpanElement>(null);
  const mounted = useRef(true);
  const savedReview = useRef<Review | null>(null);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      if (savedReview.current) void window.intercat.cancelExternal(savedReview.current.id).catch(() => {});
    };
  }, []);
  const target = destination(url, kind);
  if (!target) return <span>{kind === 'image' ? `[Image: ${label || url}]` : label || url}</span>;
  async function prepare() {
    if (working.current) return;
    working.current = true; setBusy(true); setError('');
    try {
      const value = await window.intercat.reviewExternal({ kind, url });
      if (!mounted.current) { void window.intercat.cancelExternal(value.id).catch(() => {}); return; }
      savedReview.current = value; setReview(value);
    } catch (failure) { if (mounted.current) setError(failure instanceof Error ? failure.message : 'This URL could not be checked.'); }
    finally { working.current = false; if (mounted.current) setBusy(false); }
  }
  function cancel() {
    if (savedReview.current) void window.intercat.cancelExternal(savedReview.current.id).catch(() => {});
    savedReview.current = null; setReview(null);
    requestAnimationFrame(() => trigger.current?.focus());
  }
  async function confirm() {
    if (!review || working.current) return;
    working.current = true; setBusy(true); setError('');
    const approved = review;
    savedReview.current = null; setReview(null);
    try {
      const result = await window.intercat.confirmExternal({ id: approved.id, kind: approved.kind });
      if (!mounted.current) return;
      if (kind === 'image') {
        if (!('dataUrl' in result) || !/^data:image\/(?:png|jpeg|gif|webp|svg\+xml);base64,[A-Za-z0-9+/=]+$/.test(result.dataUrl)) throw new Error('The response was not an image.');
        setImage(result.dataUrl);
      }
    } catch (failure) { if (mounted.current) setError(failure instanceof Error ? failure.message : 'This URL could not be opened.'); }
    finally { working.current = false; if (mounted.current) { setBusy(false); requestAnimationFrame(() => container.current?.focus()); } }
  }
  return <span ref={container} tabIndex={-1} className={kind === 'image' ? 'external-image' : 'external-link'}>
    {image ? <img src={image} alt={label} onError={() => { setImage(''); setError('This image could not be displayed.'); }} /> : <>
      {kind === 'image' && <span className="external-image-label">{label || 'External image'}<small>{target.host}</small></span>}
      {!review && <button ref={trigger} disabled={busy} onClick={() => { void prepare(); }} aria-label={kind === 'image' ? `Load image from ${target.host}` : undefined}>{busy ? 'Checking…' : kind === 'image' ? 'load' : label || url}</button>}
    </>}
    {review && <Confirmation review={review} cancel={cancel} confirm={() => { void confirm(); }} />}
    {error && <span className="external-error" role="alert">{error}</span>}
  </span>;
}

const components: Components = {
  img: ({ src, alt }) => <ExternalTarget key={src} kind="image" url={src ?? ''} label={alt ?? ''} />,
  a: ({ href, children }) => containsImage(children) ? <>
    {children}<ExternalTarget key={href} kind="link" url={href ?? ''} label={`Open ${textLabel(children) || 'link'}`} />
  </> : <ExternalTarget key={href} kind="link" url={href ?? ''} label={textLabel(children)} />,
};
export function MarkdownMessage({ text, className = '', label }: { text: string; className?: string; label?: string }) {
  return <div className={`markdown-message ${className}`} aria-label={label}>
    <Markdown remarkPlugins={[remarkGfm]} components={components}
      // Only our a/img components receive URLs; neither emits an unreviewed href/src.
      urlTransform={url => url}>{text}</Markdown>
  </div>;
}
