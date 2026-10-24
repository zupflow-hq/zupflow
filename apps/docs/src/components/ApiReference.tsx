import { type ReactNode, useMemo, useState } from "react";
import type { Endpoint, Field } from "../lib/openapi";
import "./ApiReference.css";

const CALLERS: Record<string, string> = {
  console: "Console session",
  api_key: "API key",
  connect: "Connect reference",
  public: "Public",
};

/** The contract's prose marks code with backticks: render it as code. */
function Prose({ text }: { text: string }) {
  return (
    <>
      {text.split("`").map((part, i) =>
        // Odd parts sat between backticks.
        // biome-ignore lint/suspicious/noArrayIndexKey: the parts of one string never reorder
        i % 2 ? <code key={i}>{part}</code> : <span key={i}>{part}</span>,
      )}
    </>
  );
}

/** JSON (or a curl body) colored by token: keys, strings, numbers, literals. */
function highlight(source: string): ReactNode[] {
  const out: ReactNode[] = [];
  const token =
    /("(?:[^"\\]|\\.)*")(\s*:)?|\b(true|false|null)\b|(-?\d+(?:\.\d+)?)|(\bcurl\b|\s-[A-Za-z]\b)|(https?:\/\/\S+)/g;
  let last = 0;
  for (const match of source.matchAll(token)) {
    const at = match.index ?? 0;
    if (at > last) out.push(source.slice(last, at));
    const [whole, string, colon, literal, number, flag, url] = match;
    const key = `${at}`;
    if (string && colon) {
      out.push(
        <span key={key} className="t-key">
          {string}
        </span>,
        colon,
      );
    } else if (string) {
      out.push(
        <span key={key} className="t-str">
          {string}
        </span>,
      );
    } else if (literal) {
      out.push(
        <span key={key} className="t-lit">
          {literal}
        </span>,
      );
    } else if (number) {
      out.push(
        <span key={key} className="t-num">
          {number}
        </span>,
      );
    } else if (flag) {
      out.push(
        <span key={key} className="t-flag">
          {flag}
        </span>,
      );
    } else if (url) {
      out.push(
        <span key={key} className="t-url">
          {url}
        </span>,
      );
    } else {
      out.push(whole);
    }
    last = at + whole.length;
  }
  if (last < source.length) out.push(source.slice(last));
  return out;
}

function Fields({ title, fields, type }: { title: string; fields: Field[]; type?: string }) {
  if (fields.length === 0) return null;
  return (
    <div className="zf-api-fields">
      <h4>
        {title}
        {type && <code className="zf-api-type">{type}</code>}
      </h4>
      <dl>
        {fields.map((field) => (
          <div key={field.name}>
            <dt>
              <code className="zf-api-name">{field.name}</code>
              <span className="zf-api-type">{field.type}</span>
              {field.required ? <span className="zf-api-req">required</span> : null}
            </dt>
            {field.description && (
              <dd>
                <Prose text={field.description} />
              </dd>
            )}
          </div>
        ))}
      </dl>
    </div>
  );
}

function CodePanel({ endpoint }: { endpoint: Endpoint }) {
  return (
    <div className="zf-api-code">
      <div className="zf-api-panel">
        <div className="zf-api-panel-head">
          <span className={`zf-api-method m-${endpoint.method.toLowerCase()}`}>{endpoint.method}</span>
          <code>{endpoint.path}</code>
        </div>
        <pre>{highlight(endpoint.request)}</pre>
      </div>
      {endpoint.example && (
        <div className="zf-api-panel">
          <div className="zf-api-panel-head">
            <span>Response</span>
            <span className="zf-api-status">{endpoint.status}</span>
          </div>
          <pre className="zf-api-response">{highlight(endpoint.example)}</pre>
        </div>
      )}
    </div>
  );
}

function Operation({ endpoint }: { endpoint: Endpoint }) {
  return (
    <article className="zf-api-op" id={endpoint.id}>
      <div className="zf-api-doc">
        <h3>
          <a href={`#${endpoint.id}`}>
            <Prose text={endpoint.summary} />
          </a>
        </h3>
        <p className="zf-api-meta">
          <span className={`zf-api-method m-${endpoint.method.toLowerCase()}`}>{endpoint.method}</span>
          <code className="zf-api-path">{endpoint.path}</code>
        </p>
        <p className="zf-api-chips">
          {endpoint.callers.map((caller) => (
            <span key={caller} className={`zf-api-caller c-${caller}`}>
              {CALLERS[caller] ?? caller}
            </span>
          ))}
          {endpoint.showsSecretOnce && <span className="zf-api-caller c-secret">Shows a secret once</span>}
          <code className="zf-api-opid">{endpoint.id}</code>
        </p>
        <Fields title="Path parameters" fields={endpoint.pathParams} />
        <Fields title="Query parameters" fields={endpoint.query} />
        <Fields title="Body" fields={endpoint.body} type={endpoint.bodyType} />
        <div className="zf-api-fields">
          <h4>Returns</h4>
          <p className="zf-api-returns">
            <code>{endpoint.status}</code>
            {endpoint.response ? (
              <>
                {" "}
                with <code className="zf-api-type">data: {endpoint.response}</code>
              </>
            ) : (
              " with an empty body"
            )}
          </p>
        </div>
      </div>
      <CodePanel endpoint={endpoint} />
    </article>
  );
}

/** Every endpoint of the API, grouped by resource: docs on the left, a request and its response on the right. */
export default function ApiReference({ endpoints }: { endpoints: Endpoint[] }) {
  const [query, setQuery] = useState("");
  const [caller, setCaller] = useState("all");

  const groups = useMemo(() => {
    const q = query.trim().toLowerCase();
    const matches = endpoints.filter(
      (e) =>
        (caller === "all" || e.callers.includes(caller)) &&
        (!q || `${e.method} ${e.path} ${e.summary} ${e.id} ${e.group}`.toLowerCase().includes(q)),
    );
    const byGroup = new Map<string, Endpoint[]>();
    for (const endpoint of matches) {
      byGroup.set(endpoint.group, [...(byGroup.get(endpoint.group) ?? []), endpoint]);
    }
    return [...byGroup.entries()];
  }, [endpoints, query, caller]);

  const count = groups.reduce((n, [, list]) => n + list.length, 0);
  const anchor = (group: string) => `group-${group.toLowerCase().replaceAll(" ", "-")}`;

  return (
    <div className="zf-api not-content">
      <div className="zf-api-toolbar">
        <input
          type="search"
          placeholder="Filter by path, operation or summary"
          aria-label="Filter endpoints"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
        />
        <select aria-label="Caller" value={caller} onChange={(event) => setCaller(event.target.value)}>
          <option value="all">Any caller</option>
          {Object.entries(CALLERS).map(([value, label]) => (
            <option key={value} value={value}>
              {label}
            </option>
          ))}
        </select>
        <span className="zf-api-count">
          {count} of {endpoints.length}
        </span>
      </div>
      <nav className="zf-api-groups" aria-label="Resources">
        {groups.map(([group, list]) => (
          <a key={group} href={`#${anchor(group)}`}>
            {group} <span>{list.length}</span>
          </a>
        ))}
      </nav>
      {groups.length === 0 && <p className="zf-api-empty">No endpoint matches.</p>}
      {groups.map(([group, list]) => (
        <section key={group} id={anchor(group)}>
          <h2 className="zf-api-group">{group}</h2>
          {list.map((endpoint) => (
            <Operation key={endpoint.id} endpoint={endpoint} />
          ))}
        </section>
      ))}
    </div>
  );
}
