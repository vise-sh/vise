// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// https://astro.build/config
export default defineConfig({
  site: 'https://docs.vise.sh',
  integrations: [
    starlight({
      title: 'vise',
      description:
        'Open-source background coding agents. vise runs them on hosts you control, on your model credentials, with a human on every PR.',
      logo: {
        light: './src/assets/logo-light.svg',
        dark: './src/assets/logo-dark.svg',
        alt: 'vise',
      },
      favicon: '/favicon.svg',
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/vise-sh/vise' },
      ],
      editLink: {
        baseUrl: 'https://github.com/vise-sh/vise/edit/main/docs/',
      },
      customCss: [
        '@fontsource/ibm-plex-mono/400.css',
        '@fontsource/ibm-plex-mono/600.css',
        './src/styles/custom.css',
      ],
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Introduction', slug: 'getting-started/introduction' },
            { label: 'Install', slug: 'getting-started/install' },
            { label: 'Quickstart', slug: 'getting-started/quickstart' },
            { label: 'Concepts', slug: 'getting-started/concepts' },
          ],
        },
      ],
    }),
  ],
});
