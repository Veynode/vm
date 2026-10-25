import { createCsrfMiddleware, createStart } from '@tanstack/react-start'

// Server functions only accept same-origin browser requests. Document loads
// are not checked here (a typed URL sends Sec-Fetch-Site: none); the Node host
// (server.mjs) gates every request on loopback Host and the per-launch token.
export const startInstance = createStart(() => ({
  requestMiddleware: [
    createCsrfMiddleware({
      filter: ({ request }) => new URL(request.url).pathname.startsWith('/_serverFn'),
    }),
  ],
}))
