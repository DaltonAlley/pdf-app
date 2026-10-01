// Browser acceptance instrumentation observes real requests and downloads.
window.artworkLayouts = [];
window.artworkDownloads = [];
const originalFetch = window.fetch.bind(window);
window.fetch = async (request, ...args) => {
  const url = String(request?.url ?? request);
  let body;
  if (url.endsWith('/gang-up/layout')) {
    body = request instanceof Request ? await request.clone().text() : args[0]?.body;
  }
  const response = await originalFetch(request, ...args);
  if (body && response.ok) {
    artworkLayouts.push({ request: JSON.parse(body), result: await response.clone().json() });
  }
  return response;
};
const blobs = new Map();
const createObjectURL = URL.createObjectURL.bind(URL);
URL.createObjectURL = blob => {
  const url = createObjectURL(blob);
  blobs.set(url, blob);
  return url;
};
const click = HTMLAnchorElement.prototype.click;
HTMLAnchorElement.prototype.click = function () {
  if (this.download && blobs.has(this.href)) {
    artworkDownloads.push({ name: this.download, blob: blobs.get(this.href) });
  }
  return click.call(this);
};
window.uploadOddArtwork = async (width, height, filename) => {
  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  const context = canvas.getContext('2d');
  ['#e02020', '#20b040', '#2040e0'].forEach((color, index) => {
    context.fillStyle = color;
    context.fillRect(index * width / 3, 0, width / 3 + 1, height);
  });
  context.fillStyle = '#000000';
  context.font = '20px sans-serif';
  context.fillText('TOP LEFT', 8, 28);
  context.fillText('BOTTOM RIGHT', width - 170, height - 12);
  const blob = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
  const transfer = new DataTransfer();
  transfer.items.add(new File([blob], filename, { type: 'image/png' }));
  const input = document.getElementById('file-input');
  input.files = transfer.files;
  input.dispatchEvent(new Event('change', { bubbles: true }));
};
window.latestArtworkLayout = () => artworkLayouts.at(-1);
// The workspace-level Artwork context has one owner even when the responsive rail hides Setup.
window.assertArtworkToolbarOwner = () => {
  const controls = document.querySelectorAll('.impose-artwork-controls');
  if (controls.length !== 1 || !controls[0].closest('#gang-artwork-panel') ||
      controls[0].closest('.gang-setup-fields') ||
      document.querySelector('#gang-preview-panel .impose-artwork-controls, #gang-preview-panel .impose-crop-editor')) {
    throw new Error('The Artwork context must exist exactly once in the Artwork rail and be absent from Setup and Preview');
  }
  return true;
};
window.assertArtworkContextReachable = async (selector = 'select[aria-label="Artwork fitting"]') => {
  const assertionNumber = (window.artworkContextAssertionCount ?? 0) + 1;
  window.artworkContextAssertionCount = assertionNumber;
  // Step changes focus their heading on a zero-delay task. Let that focus and the
  // resulting Setup scroll settle before measuring the toolbar overlay.
  await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  const panel = document.querySelector('#gang-artwork-panel');
  const toolbar = document.querySelector('.impose-workspace-toolbar');
  const stepper = document.querySelector('.setup-stepper');
  const actions = document.querySelector('.setup-step-actions');
  const stage = document.querySelector('.sheet-stage');
  if (!panel || !toolbar || !stepper || !actions || !stage) {
    throw new Error('The Artwork rail, sheet stage, Setup stepper, or sticky actions are missing');
  }
  if (!panel.checkVisibility()) document.querySelector('#gang-artwork-tab')?.click();
  await new Promise(resolve => requestAnimationFrame(resolve));
  assertArtworkToolbarOwner();
  const before = {
    toolbar: toolbar.getBoundingClientRect().toJSON(),
    stepper: stepper.getBoundingClientRect().toJSON(),
    actions: actions.getBoundingClientRect().toJSON(),
    stage: stage.getBoundingClientRect().toJSON(),
  };
  const fitting = panel.querySelector(selector);
  if (!fitting) throw new Error(`Artwork toolbar control is missing: ${selector}`);
  fitting.scrollIntoView({ block: 'nearest', behavior: 'instant' });
  const r = fitting.getBoundingClientRect(), p = panel.getBoundingClientRect();
  const afterActions = actions.getBoundingClientRect();
  const afterStepper = stepper.getBoundingClientRect();
  const afterStage = stage.getBoundingClientRect();
  const checks = {
    horizontalOverflow: document.documentElement.scrollWidth > innerWidth + 1,
    panelAboveViewport: p.top < -1,
    panelBelowViewport: p.bottom > innerHeight + 1,
    targetAbovePanel: r.top < p.top,
    targetBelowPanel: r.bottom > p.bottom,
    targetNotHitTestable: !document.elementsFromPoint(r.x + r.width / 2, r.y + r.height / 2).some(e => e === fitting || fitting.contains(e) || e.closest('label') === fitting.closest('label')),
    toolbarMoved: Math.abs(before.toolbar.top - toolbar.getBoundingClientRect().top) > 1,
    stepperMoved: Math.abs(before.stepper.top - afterStepper.top) > 1,
    footerMoved: Math.abs(before.actions.top - afterActions.top) > 1,
    stageMoved: Math.abs(before.stage.top - afterStage.top) > 1 || Math.abs(before.stage.height - afterStage.height) > 1,
    footerBelowViewport: afterActions.bottom > innerHeight + 1,
  };
  if (Object.values(checks).some(Boolean)) {
    throw new Error('The Artwork rail is clipped or scrolls the sticky actions away: ' +
      JSON.stringify({ assertionNumber, selector, checks, activeElement: document.activeElement?.id, target: r.toJSON(), panel: p.toJSON(), before, afterActions: afterActions.toJSON(), viewport: innerHeight }));
  }
  const action = actions.querySelector('button:not([disabled])');
  if (action) {
    const a = action.getBoundingClientRect();
    if (a.width > 0 && a.height > 0 && !action.contains(document.elementFromPoint(a.x + a.width / 2, a.y + a.height / 2))) {
      throw new Error('Long Artwork controls cover the sticky action pointer target');
    }
  }
  return true;
};
window.assertFinishedSizeControls = blank => {
  if (document.querySelector('select[aria-label="Finished size"] option[value="original"]')) {
    throw new Error('Original size is still offered in normal imposition UI');
  }
  for (const label of ['Finished width (in)', 'Finished height (in)']) {
    const input = document.querySelector(`input[aria-label="${label}"]`);
    if (!input || !input.checkVisibility() || input.getBoundingClientRect().width <= 0) {
      throw new Error(label + ' must stay visible without choosing Custom');
    }
    if (blank && input.value !== '') throw new Error(label + ' inferred a finished size from the source');
    if (!blank && !(Number(input.value) > 0)) throw new Error(label + ' did not retain the explicit finished size');
  }
  return true;
};
window.renderDownloadedArtwork = async index => {
  const form = new FormData();
  form.append('file', artworkDownloads[index].blob, 'imposed.pdf');
  form.append('target', 'png');
  form.append('pages', '1');
  form.append('dpi', '72');
  const response = await fetch('/convert', { method: 'POST', body: form });
  if (!response.ok) throw new Error('Exported PDF could not be rasterized: ' + await response.text());
  const bitmap = await createImageBitmap(await response.blob());
  const canvas = document.createElement('canvas');
  canvas.width = bitmap.width;
  canvas.height = bitmap.height;
  const context = canvas.getContext('2d', { willReadFrequently: true });
  context.drawImage(bitmap, 0, 0);
  bitmap.close();
  window.exportedArtworkRaster = { canvas, context };
  return { width: canvas.width, height: canvas.height };
};
window.assertArtworkPixel = (xInches, yInches, expected) => {
  const { context } = exportedArtworkRaster;
  const [r, g, b] = context.getImageData(Math.round(xInches * 72), Math.round(yInches * 72), 1, 1).data;
  const good = expected === 'white' ? r > 240 && g > 240 && b > 240
    : expected === 'red' ? r > 160 && r > g * 2 && r > b * 2
    : expected === 'green' ? g > 100 && g > r * 2 && g > b * 2
    : b > 160 && b > r * 2 && b > g * 2;
  if (!good) throw new Error(`Expected ${expected} at (${xInches}, ${yInches}) in exported PDF, got ${r},${g},${b}`);
};
true;
