if (location.hash === '#marker') document.documentElement.classList.add('marker');
window.__TAURI__.event.listen('active', ({ payload }) => {
  document.body.classList.toggle('active', payload);
  document.getElementById('state').textContent = payload ? 'активне' : 'неактивне';
});
