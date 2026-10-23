// 御端控制台共享鉴权逻辑：注入 Bearer 令牌、401 跳转登录、登出。
(function () {
  window.YD_TOKEN = localStorage.getItem('yd_token');
  window.YD_ROLE = localStorage.getItem('yd_role');

  if (!window.YD_TOKEN && !location.pathname.endsWith('login.html')) {
    location.href = 'login.html';
  }

  const _fetch = window.fetch;
  window.api = async function (p, opts) {
    opts = opts || {};
    const headers = Object.assign({ 'Authorization': 'Bearer ' + (window.YD_TOKEN || '') }, opts.headers || {});
    opts.headers = headers;
    const r = await _fetch(p, opts);
    if (r.status === 401) {
      localStorage.removeItem('yd_token');
      localStorage.removeItem('yd_role');
      location.href = 'login.html';
      throw new Error('unauthorized');
    }
    return r.json();
  };

  document.addEventListener('DOMContentLoaded', function () {
    const out = document.getElementById('logout');
    if (out) out.onclick = function () {
      localStorage.removeItem('yd_token');
      localStorage.removeItem('yd_role');
      location.href = 'login.html';
    };
    const roleEl = document.getElementById('role');
    if (roleEl) roleEl.textContent = '角色：' + (window.YD_ROLE || '-');
  });
})();
