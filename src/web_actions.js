(function () {
    'use strict';
    var toast = document.getElementById('toast'), current = null;
    var panel = document.createElement('div');
    panel.id = 'transfer';
    panel.className = 'm';
    panel.hidden = true;
    panel.setAttribute('role', 'status');
    panel.setAttribute('aria-live', 'polite');
    var label = document.createElement('span'), finish = document.createElement('button');
    var cancel = document.createElement('button');
    finish.type = cancel.type = 'button';
    finish.hidden = true;
    cancel.textContent = 'Cancel';
    panel.append(label, document.createTextNode(' '), finish, document.createTextNode(' '), cancel);
    toast.parentNode.insertBefore(panel, toast);

    function say(message) {
        toast.textContent = message;
        toast.hidden = false;
        clearTimeout(toast._h);
        toast._h = setTimeout(function () { toast.hidden = true; }, 5000);
        fetch('/log?m=' + encodeURIComponent(message)).catch(function () {});
    }
    function status(op, message) {
        if (current !== op) return;
        label.textContent = message;
        panel.hidden = false;
    }
    function stop() {
        if (!current || (current.nativePending && current.blob)) return;
        current.controller.abort();
        if (current.url) URL.revokeObjectURL(current.url);
        current = null;
        panel.hidden = true;
    }
    cancel.onclick = stop;
    function wait(ms, signal) {
        return new Promise(function (resolve, reject) {
            function abort() { clearTimeout(timer); reject(new DOMException('Cancelled', 'AbortError')); }
            var timer = setTimeout(function () { signal.removeEventListener('abort', abort); resolve(); }, ms);
            if (signal.aborted) abort();
            else signal.addEventListener('abort', abort, {once: true});
        });
    }
    async function download(op) {
        var url = (op.kind === 'copy' ? '/png?f=' : '/file?f=') + op.link.dataset.f;
        for (var attempt = 0; ; attempt++) {
            var controller = new AbortController(), idle, total, timedOut = false;
            function abort() { controller.abort(); }
            function timeout() { timedOut = true; controller.abort(); }
            function resetIdle() { clearTimeout(idle); idle = setTimeout(timeout, 20000); }
            op.controller.signal.addEventListener('abort', abort, {once: true});
            if (op.controller.signal.aborted) controller.abort();
            resetIdle();
            total = setTimeout(timeout, 120000);
            try {
                status(op, attempt ? 'Retrying download…' : 'Loading image…');
                var response = await fetch(url, {signal: controller.signal});
                if (!response.ok) {
                    var error = new Error('Image unavailable (' + response.status + ').');
                    error.retryable = [408, 429, 500, 502, 503, 504].indexOf(response.status) !== -1;
                    throw error;
                }
                // Read the whole response before exposing a file to Copy/Share/Open.
                // A dropped connection must never turn a partial image into a success.
                if (!response.body || !response.body.getReader) return await response.blob();
                var reader = response.body.getReader(), chunks = [], received = 0;
                var length = Number(response.headers.get('Content-Length')) || 0;
                for (;;) {
                    var chunk = await reader.read();
                    if (chunk.done) break;
                    chunks.push(chunk.value);
                    received += chunk.value.byteLength;
                    resetIdle();
                    status(op, 'Loading image… ' + Math.round(received / 1024) + ' KB' +
                        (length ? ' / ' + Math.round(length / 1024) + ' KB' : ''));
                }
                if (length && received !== length) throw new TypeError('Incomplete download.');
                return new Blob(chunks, {type: response.headers.get('Content-Type') || 'application/octet-stream'});
            } catch (error) {
                if (op.controller.signal.aborted) throw new DOMException('Cancelled', 'AbortError');
                if (attempt >= 2 || !(timedOut || error instanceof TypeError || error.retryable)) {
                    if (timedOut) throw new Error('Connection timed out. Try again when it improves.');
                    throw error;
                }
                status(op, 'Connection interrupted. Retrying…');
            } finally {
                clearTimeout(idle);
                clearTimeout(total);
                op.controller.signal.removeEventListener('abort', abort);
                controller.abort();
            }
            await wait(1000 * (attempt + 1), op.controller.signal);
        }
    }
    function ready(op, message) {
        if (current !== op) return;
        op.nativePending = false;
        cancel.disabled = false;
        status(op, message);
        finish.textContent = op.kind === 'copy' ? 'Copy now' : op.kind === 'share' ? 'Share now' : 'Open now';
        finish.hidden = false;
        finish.onclick = function () { perform(op); };
    }
    function failure(op, error) {
        if (current !== op) return;
        op.nativePending = false;
        cancel.disabled = false;
        if (error.name === 'AbortError') { panel.hidden = true; return; }
        if (op.blob && (error.name === 'NotAllowedError' || error.name === 'InvalidStateError')) {
            ready(op, 'Image ready. Tap to finish; allow clipboard access if asked.');
            return;
        }
        status(op, (op.kind === 'copy' ? 'Copy' : op.kind === 'share' ? 'Share' : 'Open') + ' failed: ' + error.message);
        finish.textContent = 'Retry';
        finish.hidden = false;
        finish.onclick = function () { start(op.link, op.kind); };
    }
    function copied(op) {
        if (current !== op) return;
        op.nativePending = false;
        cancel.disabled = false;
        panel.hidden = true;
        say(op.link.dataset.anim ? 'copied the first frame as PNG; Share sends the animation' : 'copied');
    }
    function perform(op) {
        if (current !== op || op.nativePending || !op.blob) return;
        finish.hidden = true;
        if (op.kind === 'open') {
            op.url = URL.createObjectURL(op.blob);
            // Same-tab navigation needs no popup permission and preserves back navigation.
            location.assign(op.url);
            return;
        }
        op.nativePending = true;
        cancel.disabled = true;
        status(op, op.kind === 'copy' ? 'Copying…' : 'Choose where to share…');
        try {
            if (op.kind === 'copy') {
                navigator.clipboard.write([new ClipboardItem({'image/png': op.blob})])
                    .then(function () { copied(op); }, function (error) { failure(op, error); });
            } else {
                var file = new File([op.blob], op.link.dataset.n, {type: op.blob.type});
                if (!navigator.canShare({files: [file]})) throw new Error('This browser cannot share this file type.');
                navigator.share({files: [file]}).then(function () {
                    if (current !== op) return;
                    op.nativePending = false;
                    cancel.disabled = false;
                    panel.hidden = true;
                    say('shared ' + op.link.dataset.n);
                }, function (error) { failure(op, error); });
            }
        } catch (error) { failure(op, error); }
    }
    function start(link, kind) {
        if (current && current.nativePending) { say('Finish the current browser action first.'); return; }
        stop();
        var op = {link: link, kind: kind, controller: new AbortController(), blob: null, nativePending: false};
        current = op;
        finish.hidden = true;
        cancel.disabled = false;
        var data = download(op).then(function (blob) { op.blob = blob; return blob; });
        // Modern ClipboardItem accepts a promise: call write DURING the tap, before
        // a slow download can expire activation and trigger a new permission prompt.
        if (kind === 'copy') {
            var item;
            try {
                item = new ClipboardItem({'image/png': data});
            } catch (_) {
                // Older Chromium rejects promise-valued ClipboardItems; fetch first
                // and require a fresh tap when activation has expired.
            }
            if (item) {
                op.nativePending = true;
                // Cancellation remains available while the download is pending.
                data.then(function () { if (current === op) cancel.disabled = true; }, function () {});
                function rejected(error) {
                    data.then(function () { failure(op, error); }, function (downloadError) { failure(op, downloadError); });
                }
                try {
                    navigator.clipboard.write([item]).then(function () { copied(op); }, rejected);
                } catch (error) { rejected(error); }
                return;
            }
        }
        data.then(function () {
            if (current !== op) return;
            if (kind === 'open' || (navigator.userActivation && navigator.userActivation.isActive)) perform(op);
            else ready(op, 'Image ready. Tap to finish.');
        }, function (error) { failure(op, error); });
    }
    document.addEventListener('click', function (event) {
        var link = event.target.closest('a.s,a.sh,a.o');
        if (!link) return;
        var kind = link.classList.contains('s') ? 'copy' : link.classList.contains('sh') ? 'share' : 'open';
        if (kind === 'share' && (!navigator.share || !navigator.canShare)) return;
        event.preventDefault();
        if (kind === 'copy' && (!navigator.clipboard || !navigator.clipboard.write || !window.ClipboardItem)) {
            say('This browser cannot copy images; use Share instead.'); return;
        }
        // Open links use the same indexed-file key as Share/Copy; retain href for no-script use.
        if (kind === 'open') link.dataset.f = new URL(link.href).searchParams.get('f');
        start(link, kind);
    });
    window.addEventListener('pagehide', function () {
        if (current) current.controller.abort();
    });
})();
