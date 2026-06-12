const ASSETS_CACHE = 'tune-assets';

self.addEventListener('install', () => {
  self.skipWaiting();
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys().then(keys =>
      Promise.all(keys.filter(k => k !== ASSETS_CACHE).map(k => caches.delete(k)))
    )
  );
  self.clients.claim();
});

self.addEventListener('fetch', (e) => {
  // Skip API, WebSocket, and streaming
  if (e.request.url.includes('/api/') || e.request.url.includes('/ws') || e.request.url.includes('/stream/')) return;

  // HTML pages: always network, never cache
  if (e.request.mode === 'navigate' || e.request.url.endsWith('/')) {
    e.respondWith(fetch(e.request));
    return;
  }

  // Assets (JS/CSS with content hashes): cache-first (immutable)
  if (e.request.url.match(/\.[a-f0-9]{8}\.(js|css)$/)) {
    e.respondWith(
      caches.match(e.request).then(cached => cached || fetch(e.request).then(resp => {
        caches.open(ASSETS_CACHE).then(c => c.put(e.request, resp.clone()));
        return resp;
      }))
    );
    return;
  }

  // Everything else: network-first
  e.respondWith(
    fetch(e.request).catch(() => caches.match(e.request))
  );
});

// Track change notifications — posted from the main thread when the tab is
// hidden. Using the service worker ensures the notification shows even if
// the browser has throttled or suspended the page.
self.addEventListener('message', (e) => {
  if (e.data && e.data.type === 'TRACK_NOTIFICATION') {
    const { title, body, icon } = e.data;
    self.registration.showNotification(title, {
      body: body || undefined,
      icon: icon || undefined,
      tag: 'tune-track-change',
      silent: true,
      renotify: true,
    });
  }
});

// Clicking a notification focuses the existing Tune tab (or opens one)
self.addEventListener('notificationclick', (e) => {
  e.notification.close();
  e.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((clients) => {
      for (const client of clients) {
        if (client.url.includes(self.location.origin) && 'focus' in client) {
          return client.focus();
        }
      }
      return self.clients.openWindow('/');
    })
  );
});
