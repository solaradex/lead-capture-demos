/* ---------------------------------------------------------------------------
 * LeadCapture chat widget — embeddable AI receptionist for local businesses.
 *
 * Usage: add ONE script tag to any page:
 *
 *   <script src="https://your-server/widget/chat-widget.js"
 *           data-business-name="Bright Spark Electric"
 *           data-agent-name="Sam"
 *           data-backend="https://your-server"></script>
 *
 * Optional data attributes:
 *   data-business-name  Business shown in the chat header (default "us")
 *   data-agent-name     Receptionist name (default "Sam")
 *   data-backend        API origin (defaults to the script's own origin)
 *   data-accent         Header/bubble color (default "#0ea5e9")
 *
 * Exposes window.LeadChat.open() / .close() / .toggle() so site buttons
 * (e.g. "Chat with us") can open the panel programmatically.
 * ------------------------------------------------------------------------- */
(function () {
  'use strict';

  var scriptTag = document.currentScript;
  var BACKEND = scriptTag.getAttribute('data-backend') ||
                new URL(scriptTag.src).origin;
  var BUSINESS = scriptTag.getAttribute('data-business-name') || 'us';
  var AGENT = scriptTag.getAttribute('data-agent-name') || 'Sam';
  var ACCENT = scriptTag.getAttribute('data-accent') || '#0ea5e9';

  var sessionId = 'sess-' + Math.random().toString(36).slice(2) +
                  Date.now().toString(36);
  var opened = false;
  var started = false;   // have we fetched the greeting yet?
  var waiting = false;   // waiting on a bot reply

  /* -- styles (injected so the host page needs no extra CSS) -- */
  var css = [
    '#lc-bubble{position:fixed;bottom:24px;right:24px;width:60px;height:60px;',
    'border-radius:50%;background:' + ACCENT + ';border:none;cursor:pointer;',
    'box-shadow:0 4px 16px rgba(0,0,0,.25);z-index:2147483000;',
    'display:flex;align-items:center;justify-content:center;}',
    '#lc-bubble svg{width:28px;height:28px;fill:#fff;}',
    '#lc-bubble .lc-dot{position:absolute;top:2px;right:2px;width:14px;height:14px;',
    'background:#22c55e;border:3px solid #fff;border-radius:50%;}',
    '#lc-panel{position:fixed;bottom:100px;right:24px;width:380px;max-width:calc(100vw - 32px);',
    'height:520px;max-height:calc(100vh - 140px);background:#fff;border-radius:16px;',
    'box-shadow:0 8px 40px rgba(0,0,0,.25);z-index:2147483001;',
    'display:none;flex-direction:column;overflow:hidden;',
    'font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;}',
    '#lc-panel.lc-open{display:flex;}',
    '#lc-head{background:' + ACCENT + ';color:#fff;padding:14px 16px;}',
    '#lc-head b{font-size:15px;display:block;}',
    '#lc-head span{font-size:12px;opacity:.9;}',
    '#lc-head .lc-online{display:inline-block;width:8px;height:8px;background:#4ade80;',
    'border-radius:50%;margin-right:6px;}',
    '#lc-close{position:absolute;top:10px;right:12px;background:none;border:none;',
    'color:#fff;font-size:20px;cursor:pointer;}',
    '#lc-msgs{flex:1;overflow-y:auto;padding:16px;display:flex;flex-direction:column;gap:10px;',
    'background:#f8fafc;}',
    '.lc-msg{max-width:82%;padding:10px 14px;border-radius:16px;font-size:14px;line-height:1.45;}',
    '.lc-bot{background:#fff;border:1px solid #e2e8f0;color:#0f172a;border-bottom-left-radius:4px;',
    'align-self:flex-start;white-space:pre-line;}',
    '.lc-user{background:' + ACCENT + ';color:#fff;border-bottom-right-radius:4px;align-self:flex-end;}',
    '.lc-typing{align-self:flex-start;background:#fff;border:1px solid #e2e8f0;',
    'border-radius:16px;padding:12px 16px;display:flex;gap:5px;}',
    '.lc-typing i{width:7px;height:7px;background:#94a3b8;border-radius:50%;',
    'animation:lc-bounce 1.2s infinite;}',
    '.lc-typing i:nth-child(2){animation-delay:.15s;}',
    '.lc-typing i:nth-child(3){animation-delay:.3s;}',
    '@keyframes lc-bounce{0%,60%,100%{transform:translateY(0);}30%{transform:translateY(-5px);}}',
    '#lc-inputrow{display:flex;border-top:1px solid #e2e8f0;padding:10px;background:#fff;}',
    '#lc-input{flex:1;border:1px solid #e2e8f0;border-radius:999px;padding:10px 16px;',
    'font-size:14px;outline:none;}',
    '#lc-input:focus{border-color:' + ACCENT + ';}',
    '#lc-send{background:' + ACCENT + ';color:#fff;border:none;border-radius:50%;width:40px;height:40px;',
    'margin-left:8px;cursor:pointer;font-size:16px;flex-shrink:0;}',
    '#lc-send:disabled{opacity:.5;cursor:default;}'
  ].join('\n');

  var style = document.createElement('style');
  style.textContent = css;
  document.head.appendChild(style);

  /* -- DOM -- */
  var bubble = document.createElement('button');
  bubble.id = 'lc-bubble';
  bubble.setAttribute('aria-label', 'Chat with us');
  bubble.innerHTML =
    '<svg viewBox="0 0 24 24"><path d="M20 2H4a2 2 0 0 0-2 2v18l4-4h14a2 2 0 0 0 2-2V4a2 2 0 0 0-2-2z"/></svg>' +
    '<span class="lc-dot"></span>';

  var panel = document.createElement('div');
  panel.id = 'lc-panel';
  panel.innerHTML =
    '<div id="lc-head" style="position:relative">' +
      '<b>' + escapeHtml(BUSINESS) + '</b>' +
      '<span><span class="lc-online"></span>' + escapeHtml(AGENT) +
      ' is online — typically replies instantly</span>' +
      '<button id="lc-close" aria-label="Close chat">&times;</button>' +
    '</div>' +
    '<div id="lc-msgs"></div>' +
    '<div id="lc-inputrow">' +
      '<input id="lc-input" type="text" placeholder="Type your message…" autocomplete="off">' +
      '<button id="lc-send" aria-label="Send">&#10148;</button>' +
    '</div>';

  document.body.appendChild(bubble);
  document.body.appendChild(panel);

  var msgs = panel.querySelector('#lc-msgs');
  var input = panel.querySelector('#lc-input');
  var sendBtn = panel.querySelector('#lc-send');

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }

  function addMsg(text, who) {
    var div = document.createElement('div');
    div.className = 'lc-msg lc-' + who;
    div.textContent = text;   // textContent: no HTML injection from bot/user
    msgs.appendChild(div);
    msgs.scrollTop = msgs.scrollHeight;
    return div;
  }

  function showTyping() {
    var t = document.createElement('div');
    t.className = 'lc-typing';
    t.id = 'lc-typing';
    t.innerHTML = '<i></i><i></i><i></i>';
    msgs.appendChild(t);
    msgs.scrollTop = msgs.scrollHeight;
  }

  function hideTyping() {
    var t = document.getElementById('lc-typing');
    if (t) t.remove();
  }

  function post(message) {
    if (waiting) return;
    waiting = true;
    showTyping();
    fetch(BACKEND + '/api/chat', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ session_id: sessionId, message: message })
    })
      .then(function (r) { return r.json(); })
      .then(function (data) {
        hideTyping();
        waiting = false;
        if (data.reply) addMsg(data.reply, 'bot');
        if (data.lead_captured) {
          var note = document.createElement('div');
          note.className = 'lc-msg lc-bot';
          note.textContent = '✅ Request received — check your phone for our call!';
          msgs.appendChild(note);
          msgs.scrollTop = msgs.scrollHeight;
        }
      })
      .catch(function () {
        hideTyping();
        waiting = false;
        addMsg('Sorry — I lost connection for a moment. Please try again.', 'bot');
      });
  }

  function send() {
    var text = input.value.trim();
    if (!text || waiting) return;
    addMsg(text, 'user');
    input.value = '';
    post(text);
  }

  function open() {
    opened = true;
    panel.classList.add('lc-open');
    if (!started) {
      started = true;
      post('__init__');   // fetch the greeting from the backend
    }
    setTimeout(function () { input.focus(); }, 100);
  }

  function close() {
    opened = false;
    panel.classList.remove('lc-open');
  }

  bubble.addEventListener('click', function () { opened ? close() : open(); });
  panel.querySelector('#lc-close').addEventListener('click', close);
  sendBtn.addEventListener('click', send);
  input.addEventListener('keydown', function (e) {
    if (e.key === 'Enter') send();
  });

  // Public API for the host page (e.g. a "Chat with us" button).
  window.LeadChat = { open: open, close: close,
                      toggle: function () { opened ? close() : open(); } };
})();
