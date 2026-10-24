import openapi from "@zupflow/contract/openapi.json";

/** The generated OpenAPI document, served beside the reference at `/openapi.json`. */
export function GET() {
  return new Response(JSON.stringify(openapi, null, 2), { headers: { "Content-Type": "application/json" } });
}
