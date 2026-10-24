import react from "@astrojs/react";
import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";

export default defineConfig({
  site: "https://docs.zupflow.com",
  redirects: {
    "/self-hosting/extending/": "/self-hosting/architecture/extending/",
    "/architecture/overview/": "/self-hosting/architecture/overview/",
    "/architecture/request-path/": "/self-hosting/architecture/request-path/",
    "/architecture/run-engine/": "/self-hosting/architecture/run-engine/",
    "/architecture/background-work/": "/self-hosting/architecture/background-work/",
    "/architecture/traits/": "/self-hosting/architecture/traits/",
    "/architecture/extending/": "/self-hosting/architecture/extending/",
  },
  integrations: [
    starlight({
      title: "Zupflow",
      description:
        "The backend for adding AI agents to your product: isolated workspaces, connected accounts, granted tools and durable runs.",
      logo: {
        light: "./src/assets/logo-light.svg",
        dark: "./src/assets/logo-dark.svg",
        replacesTitle: true,
      },
      favicon: "/favicon.svg",
      components: { Head: "./src/components/Head.astro" },
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/zupflow-hq/zupflow" }],
      editLink: { baseUrl: "https://github.com/zupflow-hq/zupflow/edit/main/apps/docs/" },
      lastUpdated: true,
      customCss: [
        "@fontsource-variable/instrument-sans",
        "@fontsource-variable/jetbrains-mono",
        "./src/styles/theme.css",
      ],
      expressiveCode: {
        themes: ["github-dark-default", "github-light-default"],
        styleOverrides: { borderRadius: "0.5rem" },
      },
      sidebar: [
        {
          label: "Start here",
          items: ["introduction", "quickstart", "how-it-works"],
        },
        { label: "Concepts", items: [{ autogenerate: { directory: "concepts" } }] },
        { label: "Guides", items: [{ autogenerate: { directory: "guides" } }] },
        { label: "Reference", items: [{ autogenerate: { directory: "reference" } }] },
        {
          label: "Self-hosting",
          items: [
            "self-hosting/overview",
            "self-hosting/server",
            "self-hosting/configuration",
            { label: "Architecture", items: [{ autogenerate: { directory: "self-hosting/architecture" } }] },
          ],
        },
        { label: "Contributing", items: [{ autogenerate: { directory: "contributing" } }] },
      ],
    }),
    react(),
  ],
});
