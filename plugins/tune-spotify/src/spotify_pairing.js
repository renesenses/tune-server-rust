'use strict';
const base = '/api/v1/streaming/spotify';
const statusText = document.getElementById('status');
const start = document.getElementById('start');
const logout = document.getElementById('logout');
let busy = false;
let timer;
async function request(path, method = 'GET') {
  const response = await fetch(base + path, {
    method, cache: 'no-store', signal: AbortSignal.timeout(8000),
    ...(method === 'POST' ? {headers: {'Content-Type': 'application/json'}, body: '{}'} : {})
  });
  if (!response.ok) throw new Error(await response.text());
  return response.json();
}
function show(result) {
  const details = result.auth_details || {};
  start.disabled = Boolean(result.authenticated || details.pairing);
  logout.hidden = !result.authenticated;
  statusText.textContent = result.authenticated
    ? `Connecté${result.username ? ' : ' + result.username : ''}. Tu peux revenir dans Tune.`
    : details.error || result.message || (details.pairing ? 'En attente : sélectionne Tune — Spotify pairing dans Spotify.' : 'Prêt à ouvrir l’appairage.');
  if (result.authenticated) clearInterval(timer);
}
async function poll() {
  if (busy) return;
  busy = true;
  try { show(await request('/auth/status')); }
  catch (error) { statusText.textContent = `Statut indisponible : ${error.message}`; }
  finally { busy = false; }
}
start.addEventListener('click', async () => {
  if (busy) return;
  busy = true;
  start.disabled = true;
  try {
    show(await request('/auth', 'POST'));
    clearInterval(timer);
    timer = setInterval(poll, 2000);
  } catch (error) {
    statusText.textContent = `Appairage impossible : ${error.message}`;
    start.disabled = false;
  } finally { busy = false; }
});
logout.addEventListener('click', async () => {
  if (busy) return;
  busy = true;
  let disconnected = false;
  try { await request('/logout', 'POST'); disconnected = true; }
  catch (error) { statusText.textContent = `Déconnexion impossible : ${error.message}`; }
  finally { busy = false; }
  if (disconnected) await poll();
});
timer = setInterval(poll, 2000);
poll();
