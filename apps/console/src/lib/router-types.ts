import "@tanstack/react-router";

declare module "@tanstack/react-router" {
  interface StaticDataRouteOption {
    /** Breadcrumb label. `$workspace` renders the current workspace's name. */
    crumb?: string;
  }
}
