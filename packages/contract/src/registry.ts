/**
 * The contract's own registry of named schemas. Each one becomes a component
 * in `openapi.json`. Kept apart from zod's global registry so nothing else
 * loaded in the same process can add components or collide with their names.
 */
import { z } from "zod";

export const registry = z.registry<{ id: string }>();
