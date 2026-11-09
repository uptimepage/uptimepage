// Copy buttons, and secret boxes: [data-secret] holds a [data-secret-masked]
// view and the [data-secret-full] value, with an optional [data-reveal] toggle.
function showSecret(box, shown) {
    box.querySelector('[data-secret-masked]').hidden = shown;
    box.querySelector('[data-secret-full]').hidden = !shown;
    const toggle = box.querySelector('[data-reveal]');
    if (toggle) {
        toggle.setAttribute('aria-pressed', String(shown));
        toggle.textContent = shown ? 'hide' : 'show';
        if (toggle.dataset.what) toggle.setAttribute('aria-label', (shown ? 'Hide the ' : 'Show the ') + toggle.dataset.what);
    }
}

// The clipboard API exists only in a secure context; plain http still allows
// the legacy command while the click lasts.
function legacyCopy(text) {
    const area = document.createElement('textarea');
    area.value = text;
    area.setAttribute('readonly', '');
    area.style.position = 'fixed';
    area.style.opacity = '0';
    document.body.appendChild(area);
    area.select();
    let ok = false;
    try { ok = document.execCommand('copy'); } catch { ok = false; }
    area.remove();
    return ok;
}

function copied(btn) {
    const label = btn.querySelector('[data-copy-label]') || btn;
    if (!label.dataset.copyOriginal) label.dataset.copyOriginal = label.textContent;
    label.textContent = btn.dataset.copyDone || 'copied';
    clearTimeout(label.copyTimer);
    label.copyTimer = setTimeout(function () { label.textContent = label.dataset.copyOriginal; }, 1500);
}

// Nothing could copy: show the text, selected, to copy by hand.
function selectForHand(target) {
    const box = target.closest('[data-secret]');
    if (box) showSecret(box, true);
    else target.hidden = false;
    const selection = window.getSelection();
    if (selection) selection.selectAllChildren(target);
}

document.addEventListener('click', function (e) {
    const toggle = e.target.closest('[data-reveal]');
    if (toggle) {
        showSecret(toggle.closest('[data-secret]'), toggle.getAttribute('aria-pressed') !== 'true');
        return;
    }
    const btn = e.target.closest('[data-copy]');
    if (!btn) return;
    const target = document.querySelector(btn.getAttribute('data-copy'));
    if (!target) return;
    const text = target.textContent.trim();
    // Called in the click itself: Safari drops the gesture across a microtask.
    if (!navigator.clipboard) {
        if (legacyCopy(text)) copied(btn);
        else selectForHand(target);
        return;
    }
    // A blocked frame can throw instead of rejecting.
    try {
        navigator.clipboard.writeText(text).then(function () { copied(btn); }, function () { selectForHand(target); });
    } catch {
        if (legacyCopy(text)) copied(btn);
        else selectForHand(target);
    }
});
