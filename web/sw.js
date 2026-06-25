const CACHE_NAME = 'tune-v2';
const PRECACHE = ['/', '/index.html'];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE_NAME).then(c => c.addAll(PRECACHE)));
  self.skipWaiting();
});

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches.keys().then(keys =>
      Promise.all(keys.filter(k => k !== CACHE_NAME).map(k => caches.delete(k)))
    )
  );
  self.clients.claim();
});

self.addEventListener('fetch', (e) => {
  if (e.request.url.includes('/api/') || e.request.url.includes('/ws') || e.request.url.includes('/stream/')) return;
  // Network-first: always try the network, fall back to cache offline
  e.respondWith(
    fetch(e.request)
      .then(resp => {
        if (resp.ok) {
          const clone = resp.clone();
          caches.open(CACHE_NAME).then(c => c.put(e.request, clone));
        }
        return resp;
      })
      .catch(() => caches.match(e.request))
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
