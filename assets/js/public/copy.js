document.addEventListener('click', function (e) {
    const btn = e.target.closest('[data-copy]');
    if (!btn) return;
    const target = document.querySelector(btn.getAttribute('data-copy'));
    if (!target) return;
    const text = target.textContent.trim();
    // Called in the click itself: Safari drops the gesture across a microtask.
    let copied;
    try { copied = navigator.clipboard.writeText(text); } catch (err) { copied = Promise.reject(err); }
    copied.then(function () {
        const label = btn.querySelector('[data-copy-label]') || btn;
        if (!label.dataset.copyOriginal) label.dataset.copyOriginal = label.textContent;
        label.textContent = btn.dataset.copyDone || 'copied';
        clearTimeout(label.copyTimer);
        label.copyTimer = setTimeout(function () { label.textContent = label.dataset.copyOriginal; }, 1500);
    }, function () {
        // No clipboard (plain http, or permission refused): show the text to copy by hand.
        target.hidden = false;
        const selection = window.getSelection();
        if (selection) selection.selectAllChildren(target);
    });
});
