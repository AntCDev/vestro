import { defineConfig } from 'vite';
import tailwindcss from '@tailwindcss/vite';
import { resolve } from 'path';

export default defineConfig({
  envDir: '../',
  plugins: [
    tailwindcss(),
  ],
  build: {
    outDir: '../wwwroot',
    emptyOutDir: true,
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        ledger: resolve(__dirname, 'ledger.html'),
        new_invoice: resolve(__dirname, 'new-invoice.html'),
        invoices: resolve(__dirname, 'invoices.html'),
        checkoutSol: resolve(__dirname, 'checkout/SOL.html'),
        checkoutEvm: resolve(__dirname, 'checkout/EVM.html'),
      },
    },
  },
  resolve: {
    alias: {
      buffer: 'buffer/',
    },
  },
  optimizeDeps: {
    include: ['buffer'],
  },
  define: {
    global: 'globalThis',
  },
  server: {
    proxy: {
      '/api': 'http://localhost:8080',
    },
  },
});