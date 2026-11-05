import { getDocument, PDFWorker } from 'pdfjs-dist';
import PdfWorker from 'pdfjs-dist/build/pdf.worker.min.mjs?worker';

// A complete, local one-page fixture. It deliberately exercises fonts, text
// extraction, and graphics without a remote document or auxiliary font fetch.
function fixture(): Uint8Array {
  const content = 'q 0.20 0.35 0.55 rg 20 20 220 60 re f Q\nBT /F1 17 Tf 28 100 Td (Intercat local PDF) Tj ET\n';
  const objects = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 280 140] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>',
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
    `<< /Length ${content.length} >>\nstream\n${content}endstream`,
  ];
  let pdf = '%PDF-1.4\n';
  const offsets = [0];
  objects.forEach((object, i) => { offsets.push(pdf.length); pdf += `${i + 1} 0 obj\n${object}\nendobj\n`; });
  const xref = pdf.length;
  pdf += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  offsets.slice(1).forEach(offset => { pdf += `${String(offset).padStart(10, '0')} 00000 n \n`; });
  pdf += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return new TextEncoder().encode(pdf);
}

export async function renderFixture(canvas: HTMLCanvasElement) {
  const port = new PdfWorker();
  const worker = PDFWorker.create({ port });
  const task = getDocument({ data: fixture(), worker, useSystemFonts: true, useWorkerFetch: false });
  try {
    const pdf = await task.promise;
    const page = await pdf.getPage(1);
    const viewport = page.getViewport({ scale: 2 });
    canvas.width = viewport.width; canvas.height = viewport.height;
    await page.render({ canvas, viewport }).promise;
    const content = await page.getTextContent();
    return { pages: pdf.numPages, text: content.items.filter(item => 'str' in item).map(item => item.str).join(' '), realWorker: worker.port === port };
  } finally {
    await task.destroy(); worker.destroy(); port.terminate();
  }
}
