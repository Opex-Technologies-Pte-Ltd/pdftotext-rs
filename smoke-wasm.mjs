// Phase 1 smoke: load the wasm-pack nodejs pkg, call extract_pdf(bytes, layout),
// assert the return shape + that text decodes through wasm (WinAnsi smart quote:
// 0x92 → U+2019 — the gap #1 path). Run:
//   wasm-pack build --target nodejs --release --out-dir pkg-nodejs --out-name pdftotext && bun smoke-wasm.mjs
// (bundler-target glue needs a real bundler; nodejs target runs under bun/node.)

import { readFileSync } from 'node:fs';
import { extract_pdf, page_images } from './pkg-nodejs/pdftotext.js';

const enc = (s) => new TextEncoder().encode(s);
function concat(a, b) {
  const o = new Uint8Array(a.length + b.length);
  o.set(a);
  o.set(b, a.length);
  return o;
}

// Port of tests/gaps.rs build_pdf: a 1-page PDF with font obj 5, content obj 4,
// page obj 3, computed xref offsets.
function buildTextPdf(fontObjBody, content) {
  const offsets = [0, 0, 0, 0, 0, 0]; // index by obj number; [0] unused
  let buf = concat(enc('%PDF-1.4\n'), enc('%\xe2\xe3\xcf\xd3\n'));
  const emit = (n, bytes) => {
    offsets[n] = buf.length;
    buf = concat(buf, bytes);
  };

  emit(1, enc('1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n'));
  emit(2, enc('2 0 obj\n<</Type/Pages/Kids[3 0 R]/Count 1>>\nendobj\n'));
  emit(
    4,
    concat(
      enc(`4 0 obj\n<</Length ${content.length}>>\nstream\n`),
      concat(content, enc('\nendstream\nendobj\n')),
    ),
  );
  emit(5, enc(`5 0 obj\n${fontObjBody}\nendobj\n`));
  emit(
    3,
    enc('3 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>\nendobj\n'),
  );

  const xrefOff = buf.length;
  buf = concat(buf, enc('xref\n0 6\n0000000000 65535 f \n'));
  for (let n = 1; n <= 5; n++) {
    buf = concat(buf, enc(`${String(offsets[n]).padStart(10, '0')} 00000 n \n`));
  }
  buf = concat(buf, enc(`trailer\n<</Size 6/Root 1 0 R>>\nstartxref\n${xrefOff}\n%%EOF`));
  return buf;
}

let pass = 0;
let fail = 0;
function check(name, cond, extra = '') {
  console.log(`${cond ? 'PASS' : 'FAIL'}: ${name}${extra ? ` — ${extra}` : ''}`);
  cond ? pass++ : fail++;
}

// 1. WinAnsi font, smart quote: \222 (0x92) must decode to U+2019 through wasm,
//    and the return shape must be { pageCount, pages:[{pageNumber,text,charCount,sectionHeaders}] }.
{
  const font = '<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>';
  const content = enc('BT /F1 12 Tf (it\\222s) Tj ET'); // \222 = 0x92 = U+2019
  const res = extract_pdf(buildTextPdf(font, content), false);
  check('shape: pageCount === 1', res?.pageCount === 1, `got ${res?.pageCount}`);
  check('shape: pages is 1-element array', Array.isArray(res?.pages) && res.pages.length === 1);
  const p = res.pages[0];
  check('shape: pageNumber === 1', p?.pageNumber === 1);
  check('shape: charCount === text char length', p?.charCount === [...p.text].length);
  check('shape: sectionHeaders is array', Array.isArray(p?.sectionHeaders));
  check('text decoded through wasm (WinAnsi 0x92 → U+2019)', p?.text?.includes('it’s'), JSON.stringify(p?.text));
}

// 2. layout=true on the same PDF still decodes (text survives the layout path).
{
  const font = '<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>';
  const content = enc('BT /F1 12 Tf (hello) Tj ET');
  const res = extract_pdf(buildTextPdf(font, content), true);
  check('layout=true still decodes text', res?.pages?.[0]?.text?.includes('hello'), JSON.stringify(res?.pages?.[0]?.text));
}

// 3. Non-PDF throws a tagged JS string (the TS caller branches on the tag).
try {
  extract_pdf(enc('not a pdf'), true);
  check('non-PDF throws', false);
} catch (e) {
  check('non-PDF throws a "not_pdf:" tagged string', typeof e === 'string' && e.startsWith('not_pdf:'), String(e));
}

// 4. Zero-page doc, password-protected doc: typed errors, not partial results.
const streamObj = (dictExtra, data) => `<</Length ${data.length}${dictExtra}>>\nstream\n${data}\nendstream`;
function pdfFromObjects(objs, trailerExtra = '') {
  let buf = enc('%PDF-1.7\n');
  const offs = {};
  for (const [n, body] of objs) {
    offs[n] = buf.length;
    buf = concat(buf, enc(`${n} 0 obj\n${body}\nendobj\n`));
  }
  const size = Math.max(...objs.map(([n]) => n)) + 1;
  const xref = buf.length;
  let x = `xref\n0 ${size}\n0000000000 65535 f \n`;
  for (let n = 1; n < size; n++) {
    x += offs[n] === undefined ? '0000000000 65535 f \n' : `${String(offs[n]).padStart(10, '0')} 00000 n \n`;
  }
  return concat(buf, enc(`${x}trailer\n<</Size ${size}/Root 1 0 R ${trailerExtra}>>\nstartxref\n${xref}\n%%EOF`));
}
try {
  extract_pdf(pdfFromObjects([[1, '<</Type/Catalog/Pages 2 0 R>>'], [2, '<</Type/Pages/Kids[]/Count 0>>']]), true);
  check('zero-page doc throws', false);
} catch (e) {
  check('zero-page doc throws "unsupported:"', typeof e === 'string' && e.startsWith('unsupported:'), String(e));
}
{
  const hex = 'AB'.repeat(32);
  const pdf = pdfFromObjects(
    [
      [1, '<</Type/Catalog/Pages 2 0 R>>'],
      [2, '<</Type/Pages/Kids[3 0 R]/Count 1>>'],
      [3, '<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>'],
      [9, `<</Filter/Standard/V 1/R 2/Length 40/P -4/O <${hex}>/U <${hex}>>>`],
    ],
    '/Encrypt 9 0 R /ID [<00112233445566778899AABBCCDDEEFF><00112233445566778899AABBCCDDEEFF>]',
  );
  try {
    extract_pdf(pdf, true);
    check('password-protected doc throws', false);
  } catch (e) {
    check('password-protected doc throws "encrypted:"', typeof e === 'string' && e.startsWith('encrypted:'), String(e));
  }
}

// 5. Self-referencing form XObject terminates (no stack overflow / hang through wasm).
{
  const pdf = pdfFromObjects([
    [1, '<</Type/Catalog/Pages 2 0 R>>'],
    [2, '<</Type/Pages/Kids[3 0 R]/Count 1>>'],
    [3, '<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>/XObject<</Fm1 6 0 R>>>>>>'],
    [4, streamObj('', 'BT /F1 12 Tf (page) Tj ET /Fm1 Do')],
    [5, '<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>'],
    [6, streamObj('/Subtype/Form/Resources<</Font<</F1 5 0 R>>/XObject<</Fm1 6 0 R>>>>', 'BT /F1 12 Tf (loop) Tj ET /Fm1 Do')],
  ]);
  const t0 = Date.now();
  const res = extract_pdf(pdf, true);
  check('cyclic form terminates', Date.now() - t0 < 5000 && res.pages[0].text.includes('page'), JSON.stringify(res.pages[0].text));
}

// 6. Trap recovery, as a host loader does it (OpexMX: pdf-wasm-loader.ts), against the bundler-target glue:
//    a wasm trap poisons the instance; the loader detaches the old memory (grow(0)) and re-instantiates.
//    Needs `wasm-pack build --target bundler` output in ./pkg (scripts/build-pdftotext-wasm.sh).
try {
  const bg = await import('./pkg/pdftotext_bg.js');
  const wasmBytes = readFileSync(new URL('./pkg/pdftotext_bg.wasm', import.meta.url));
  const mod = await WebAssembly.compile(wasmBytes);
  const instantiate = async () => {
    const inst = await WebAssembly.instantiate(mod, { './pdftotext_bg.js': bg });
    bg.__wbg_set_wasm(inst.exports);
    return inst.exports;
  };
  let exp = await instantiate();
  const font = '<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>';
  const good = buildTextPdf(font, enc('BT /F1 12 Tf (recovered) Tj ET'));
  check('bundler glue: baseline extract', bg.extract_pdf(good, true).pages[0].text.includes('recovered'));
  // Force an out-of-bounds access inside wasm: a pointer far past the end of memory.
  let trapped = null;
  try {
    const retptr = exp.__wbindgen_add_to_stack_pointer(-16);
    exp.extract_pdf(retptr, 0xfffffff0, 4096, true);
  } catch (e) {
    trapped = e;
  }
  check('bad pointer traps as WebAssembly.RuntimeError', trapped instanceof WebAssembly.RuntimeError, String(trapped));
  // Loader's discardInstance + re-instantiate.
  try { exp.memory.grow(0); } catch {}
  exp = await instantiate();
  check('fresh instance after trap extracts correctly', bg.extract_pdf(good, true).pages[0].text.includes('recovered'));
  check('and a second call too (glue caches refreshed)', bg.extract_pdf(good, false).pages[0].text.includes('recovered'));
  // Detach this instance's memory so the glue's cached view does not outlive it (the loader's
  // release()); otherwise the next instance would be written through a stale view.
  try { exp.memory.grow(0); } catch {}
} catch (e) {
  check('trap-recovery section ran', false, String(e));
}

// 7. Memory ceiling (pdftotext-rs/build.rs `--max-memory`), against the bundler glue as the
//    Worker loader uses it: a document that needs more than the ceiling traps INSIDE wasm with
//    the allocation-failure flag set (the loader maps that to WasmMemoryLimitError and routes the
//    PDF to the container), and a fresh instance of the same compiled Module works afterwards.
try {
  const MAX = 80 * 1024 * 1024; // build.rs WASM_MAX_MEMORY
  const bg = await import('./pkg/pdftotext_bg.js');
  const mod = await WebAssembly.compile(readFileSync(new URL('./pkg/pdftotext_bg.wasm', import.meta.url)));
  const fresh = async () => {
    const inst = await WebAssembly.instantiate(mod, { './pdftotext_bg.js': bg });
    bg.__wbg_set_wasm(inst.exports);
    return inst.exports;
  };
  const release = (exp) => {
    try { exp.memory.grow(0); } catch {}
    bg.__wbg_set_wasm(undefined);
  };
  let exp = await fresh();
  let refused = false;
  try {
    exp.memory.grow(MAX / 65536 + 1 - exp.memory.buffer.byteLength / 65536);
  } catch {
    refused = true;
  }
  check(`memory cannot grow past the ${MAX >> 20} MiB ceiling`, refused);
  // ~40 MB uncompressed content stream: input copy + lopdf's copy exceed the ceiling.
  const line = 'BT /F1 12 Tf 72 700 Td (hello world) Tj ET\n';
  const big = buildTextPdf(
    '<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>',
    enc(line.repeat(Math.ceil((40 << 20) / line.length))),
  );
  let trapped = null;
  try {
    bg.extract_pdf(big, true);
  } catch (e) {
    trapped = e;
  }
  check('oversize document traps (RuntimeError), not OOM', trapped instanceof WebAssembly.RuntimeError, String(trapped));
  check('allocation-failure flag is set after the trap', exp.alloc_failed() === 1);
  check('trap happened at the ceiling', exp.memory.buffer.byteLength <= MAX);
  release(exp);
  exp = await fresh();
  const good = buildTextPdf(
    '<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>',
    enc('BT /F1 12 Tf (after the ceiling) Tj ET'),
  );
  check('fresh instance after a ceiling trap extracts', bg.extract_pdf(good, true).pages[0].text.includes('after the ceiling'));
  check('and its flag is clear', exp.alloc_failed() === 0);
  release(exp);
} catch (e) {
  check('memory-ceiling section ran', false, String(e));
}

// 8. page_images: a full-page DCTDecode image comes back as the stored JPEG bytes (Uint8Array).
{
  const jpeg = [0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01, 0x00, 0x02, 0x01, 0x01, 0x11, 0x00, 0xff, 0xd9];
  const imgStream = concat(
    enc(`<</Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8/ColorSpace/DeviceGray/Filter/DCTDecode/Length ${jpeg.length}>>\nstream\n`),
    concat(new Uint8Array(jpeg), enc('\nendstream')),
  );
  const objs = [
    [1, '<</Type/Catalog/Pages 2 0 R>>'],
    [2, '<</Type/Pages/Kids[3 0 R]/Count 1>>'],
    [3, '<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</XObject<</Im1 5 0 R>>>>>>'],
    [4, streamObj('', 'q 612 0 0 792 0 0 cm /Im1 Do Q')],
  ];
  let buf = enc('%PDF-1.7\n');
  const offs = {};
  for (const [n, body] of objs) {
    offs[n] = buf.length;
    buf = concat(buf, enc(`${n} 0 obj\n${body}\nendobj\n`));
  }
  offs[5] = buf.length;
  buf = concat(buf, concat(enc('5 0 obj\n'), concat(imgStream, enc('\nendobj\n'))));
  const xref = buf.length;
  let x = 'xref\n0 6\n0000000000 65535 f \n';
  for (let n = 1; n <= 5; n++) x += `${String(offs[n]).padStart(10, '0')} 00000 n \n`;
  buf = concat(buf, enc(`${x}trailer\n<</Size 6/Root 1 0 R>>\nstartxref\n${xref}\n%%EOF`));
  const res = page_images(buf, Uint32Array.from([1, 9]), 1 << 20);
  const im = res?.pages?.[0]?.images?.[0];
  check('page_images: one page (out-of-range page 9 skipped)', res?.pageCount === 1 && res.pages.length === 1);
  check('page_images: JPEG returned as stored', im?.codec === 'jpeg' && im.data instanceof Uint8Array && im.data.length === jpeg.length && im.data.every((b, i) => b === jpeg[i]));
  check('page_images: placement + coverage', Math.abs(im?.coverage - 1) < 1e-9 && im?.placed?.x1 === 612 && im?.jpegComponents === 1, JSON.stringify({ ...im, data: undefined }));
}

// 9. page_images: a JBIG2 image (embedded stream: page information + one MMR generic region,
//    a 16×8 box outline with a diagonal) comes back decoded to a 1-bit PNG.
{
  const hex =
    '000000003000010000001300000010000000080000000000000000000000000000012700010000002900000010' +
    '000000080000000000000000000126a0bca1ca23891d7891d7891d7891d788205c460020020000000231000100000000';
  const jb = Uint8Array.from(hex.match(/../g).map((h) => parseInt(h, 16)));
  const objs = [
    '<</Type/Catalog/Pages 2 0 R>>',
    '<</Type/Pages/Kids[3 0 R]/Count 1>>',
    '<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</XObject<</Im1 5 0 R>>>>>>',
    streamObj('', 'q 612 0 0 792 0 0 cm /Im1 Do Q'),
  ].map(enc);
  objs.push(
    concat(
      enc(`<</Type/XObject/Subtype/Image/Width 16/Height 8/BitsPerComponent 1/ColorSpace/DeviceGray/Filter/JBIG2Decode/Length ${jb.length}>>\nstream\n`),
      concat(jb, enc('\nendstream')),
    ),
  );
  let buf = enc('%PDF-1.7\n');
  const offs = [];
  objs.forEach((body, i) => {
    offs.push(buf.length);
    buf = concat(buf, concat(enc(`${i + 1} 0 obj\n`), concat(body, enc('\nendobj\n'))));
  });
  const xref = buf.length;
  let x = `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n`;
  for (const o of offs) x += `${String(o).padStart(10, '0')} 00000 n \n`;
  buf = concat(buf, enc(`${x}trailer\n<</Size ${objs.length + 1}/Root 1 0 R>>\nstartxref\n${xref}\n%%EOF`));
  const im = page_images(buf, Uint32Array.from([1]), 1 << 20)?.pages?.[0]?.images?.[0];
  const png = im?.data;
  const be32 = (o) => ((png[o] << 24) | (png[o + 1] << 16) | (png[o + 2] << 8) | png[o + 3]) >>> 0;
  check(
    'page_images: JBIG2 decoded to a 1-bit PNG of the image size',
    im?.codec === 'jbig2' && im.format === 'png' && png instanceof Uint8Array &&
      png[1] === 0x50 && png[2] === 0x4e && png[3] === 0x47 && be32(16) === 16 && be32(20) === 8 && png[24] === 1,
    JSON.stringify({ ...im, data: undefined }),
  );
}

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail === 0 ? 0 : 1);
