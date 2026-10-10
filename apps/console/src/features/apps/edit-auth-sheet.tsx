import { zodResolver } from "@hookform/resolvers/zod";
import type { AppDefinition, AuthTemplate } from "@zupflow/sdk";
import { PlusIcon, Trash2Icon } from "lucide-react";
import { useEffect } from "react";
import { type FieldPath, type UseFormReturn, useFieldArray, useForm } from "react-hook-form";
import { z } from "zod";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Sheet, SheetContent, SheetDescription, SheetFooter, SheetHeader, SheetTitle } from "@/components/ui/sheet";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { applyFieldErrors, toastError } from "@/lib/errors";

/** A URL field: blank means none. The server checks it's public HTTPS. */
const optionalUrl = z
  .string()
  .trim()
  .refine((value) => value === "" || /^https:\/\//i.test(value), "Use an https:// URL, or leave it blank.");

const oauthSchema = z.object({
  authorize_url: optionalUrl,
  token_url: optionalUrl,
  identity_url: optionalUrl,
  pkce: z.boolean(),
  scopes: z
    .array(
      z.object({ name: z.string().trim().min(1, "Name the scope.").max(300), description: z.string().trim().max(300) }),
    )
    .max(50, "Up to 50 scopes."),
});

const tokenSchema = z.object({
  inputs: z
    .array(
      z.object({
        key: z.string().regex(/^[a-z][a-z0-9_]{0,39}$/, "A lowercase letter, then letters, digits or _."),
        label: z.string().trim().min(1, "Label it.").max(60),
        secret: z.boolean(),
        placeholder: z.string().trim().max(200),
        help: z.string().trim().max(500),
        /** Not edited here; it travels with its row, and a new row has none. */
        pattern: z.string(),
      }),
    )
    .min(1, "Ask for at least one value.")
    .max(10, "Up to 10 values."),
  header: z
    .string()
    .trim()
    .regex(/^[!#$%&'*+\-.^_`|~0-9A-Za-z]{1,60}$/, "Use a header name, like Authorization."),
  prefix: z.string().max(40, "Use up to 40 characters."),
  verify_url: optionalUrl,
  instructions: z.string().trim().max(1000),
  docs_url: optionalUrl,
});

type OAuthValues = z.infer<typeof oauthSchema>;
type TokenValues = z.infer<typeof tokenSchema>;

const orNull = (value: string) => (value.trim() === "" ? null : value.trim());

/** Where each server field lands in the form. */
const OAUTH_FIELDS: Record<string, FieldPath<OAuthValues>> = {
  "auth.authorize_url": "authorize_url",
  "auth.token_url": "token_url",
  "auth.identity_url": "identity_url",
  "auth.pkce": "pkce",
  "auth.available_scopes": "scopes",
};
const TOKEN_FIELDS: Record<string, FieldPath<TokenValues>> = {
  "auth.inputs": "inputs",
  "auth.placement.name": "header",
  "auth.placement.prefix": "prefix",
  "auth.verify_url": "verify_url",
  "auth.instructions": "instructions",
  "auth.docs_url": "docs_url",
};

type Save = (auth: AuthTemplate, callbacks: { onSuccess: () => void; onError: (error: unknown) => void }) => void;

/**
 * Edits how an app's accounts authenticate: an OAuth app's endpoints and the
 * scopes it offers, or what a token app asks for and where it sends it. The
 * type itself is fixed when the app is created.
 */
export function EditAuthSheet({
  app,
  open,
  onOpenChange,
  onSave,
  pending,
}: {
  app: AppDefinition;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSave: Save;
  pending: boolean;
}) {
  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent className="w-full gap-0 sm:max-w-lg">
        <SheetHeader className="border-b">
          <SheetTitle>Authentication</SheetTitle>
          <SheetDescription>
            {app.auth.type === "oauth2"
              ? "Where Connect sends people to approve access, and the scopes configurations can ask for."
              : "What Connect asks people for, and how Zupflow sends it to the server."}
          </SheetDescription>
        </SheetHeader>
        {app.auth.type === "oauth2" ? (
          <OAuthForm
            auth={app.auth}
            open={open}
            onSave={onSave}
            onCancel={() => onOpenChange(false)}
            pending={pending}
          />
        ) : app.auth.type === "token" ? (
          <TokenForm
            auth={app.auth}
            open={open}
            onSave={onSave}
            onCancel={() => onOpenChange(false)}
            pending={pending}
          />
        ) : null}
      </SheetContent>
    </Sheet>
  );
}

function RemoveButton({ label, disabled, onClick }: { label: string; disabled?: boolean; onClick: () => void }) {
  return (
    <Button type="button" variant="ghost" size="icon" aria-label={label} disabled={disabled} onClick={onClick}>
      <Trash2Icon />
    </Button>
  );
}

function Footer({ form, onCancel, pending }: { form: string; onCancel: () => void; pending: boolean }) {
  return (
    <SheetFooter className="flex-row justify-end border-t">
      <Button variant="outline" onClick={onCancel}>
        Cancel
      </Button>
      <Button type="submit" form={form} disabled={pending}>
        {pending ? <Spinner /> : null}
        Save authentication
      </Button>
    </SheetFooter>
  );
}

/** A list's own error: the server's, or the client's on the array itself. */
function ListMessage<T extends OAuthValues | TokenValues>({
  form,
  name,
}: {
  form: UseFormReturn<T>;
  name: "scopes" | "inputs";
}) {
  const error = (
    form.formState.errors as Record<string, { message?: string; root?: { message?: string } } | undefined>
  )[name];
  const message = error?.message ?? error?.root?.message;
  return message ? <p className="text-sm text-destructive">{message}</p> : null;
}

function UrlField<T extends OAuthValues | TokenValues>({
  form,
  name,
  label,
  description,
  placeholder,
}: {
  form: UseFormReturn<T>;
  name: FieldPath<T>;
  label: string;
  description?: string;
  placeholder: string;
}) {
  return (
    <FormField
      control={form.control}
      name={name}
      render={({ field }) => (
        <FormItem>
          <FormLabel>{label}</FormLabel>
          <FormControl>
            <Input
              className="font-mono text-sm"
              placeholder={placeholder}
              autoComplete="off"
              spellCheck={false}
              {...field}
              value={field.value as string}
            />
          </FormControl>
          {description ? <FormDescription>{description}</FormDescription> : null}
          <FormMessage />
        </FormItem>
      )}
    />
  );
}

function OAuthForm({
  auth,
  open,
  onSave,
  onCancel,
  pending,
}: {
  auth: Extract<AuthTemplate, { type: "oauth2" }>;
  open: boolean;
  onSave: Save;
  onCancel: () => void;
  pending: boolean;
}) {
  const defaults = (): OAuthValues => ({
    authorize_url: auth.authorize_url ?? "",
    token_url: auth.token_url ?? "",
    identity_url: auth.identity_url ?? "",
    pkce: auth.pkce,
    scopes: auth.available_scopes.map((scope) => ({ ...scope })),
  });
  const form = useForm<OAuthValues>({ resolver: zodResolver(oauthSchema), defaultValues: defaults() });
  const scopes = useFieldArray({ control: form.control, name: "scopes" });
  // biome-ignore lint/correctness/useExhaustiveDependencies: reset to the stored template each time the sheet opens
  useEffect(() => {
    if (open) form.reset(defaults());
  }, [open]);

  return (
    <Form {...form}>
      <form
        id="edit-auth"
        className="flex-1 space-y-5 overflow-y-auto p-4"
        onSubmit={form.handleSubmit((values) =>
          onSave(
            {
              type: "oauth2",
              authorize_url: orNull(values.authorize_url),
              token_url: orNull(values.token_url),
              identity_url: orNull(values.identity_url),
              pkce: values.pkce,
              available_scopes: values.scopes,
            },
            {
              onSuccess: onCancel,
              onError: (error) => {
                if (!applyFieldErrors(form, error, OAUTH_FIELDS)) toastError(error, "Couldn't save authentication");
              },
            },
          ),
        )}
      >
        <UrlField
          form={form}
          name="authorize_url"
          label="Authorize URL"
          placeholder="https://provider.example/oauth/authorize"
        />
        <UrlField form={form} name="token_url" label="Token URL" placeholder="https://provider.example/oauth/token" />
        <UrlField
          form={form}
          name="identity_url"
          label="Account identity URL"
          placeholder="https://api.provider.example/me"
          description="Optional. A read-only request that names the connected account, so a reconnect can check it's the same one."
        />
        <FormField
          control={form.control}
          name="pkce"
          render={({ field }) => (
            <FormItem className="flex items-center justify-between gap-4 rounded-lg border p-3">
              <div className="space-y-0.5">
                <FormLabel>PKCE</FormLabel>
                <FormDescription>
                  Send an S256 code challenge. Turn off only if the provider rejects it.
                </FormDescription>
              </div>
              <FormControl>
                <Switch checked={field.value} onCheckedChange={field.onChange} />
              </FormControl>
            </FormItem>
          )}
        />
        <fieldset className="space-y-3">
          <div>
            <legend className="text-sm font-medium">Scopes offered</legend>
            <p className="text-sm text-muted-foreground">
              Each environment's configuration picks from these. A scope a configuration asks for can't be removed.
            </p>
          </div>
          {scopes.fields.length > 0 ? (
            <ul className="space-y-2">
              {scopes.fields.map((scope, index) => (
                <li key={scope.id} className="grid grid-cols-[minmax(0,1fr)_minmax(0,1.4fr)_auto] items-start gap-2">
                  <FormField
                    control={form.control}
                    name={`scopes.${index}.name`}
                    render={({ field }) => (
                      <FormItem>
                        <FormControl>
                          <Input
                            aria-label={`Scope ${index + 1}`}
                            className="font-mono text-sm"
                            placeholder="read"
                            autoComplete="off"
                            spellCheck={false}
                            {...field}
                          />
                        </FormControl>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                  <FormField
                    control={form.control}
                    name={`scopes.${index}.description`}
                    render={({ field }) => (
                      <FormItem>
                        <FormControl>
                          <Input
                            aria-label={`What scope ${index + 1} allows`}
                            placeholder="What it allows"
                            {...field}
                          />
                        </FormControl>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                  <RemoveButton label={`Remove scope ${index + 1}`} onClick={() => scopes.remove(index)} />
                </li>
              ))}
            </ul>
          ) : (
            <p className="rounded-lg border border-dashed p-3 text-sm text-muted-foreground">
              None listed: configurations request the provider's defaults.
            </p>
          )}
          <ListMessage form={form} name="scopes" />
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => scopes.append({ name: "", description: "" })}
          >
            <PlusIcon /> Add scope
          </Button>
        </fieldset>
      </form>
      <Footer form="edit-auth" onCancel={onCancel} pending={pending} />
    </Form>
  );
}

function TokenForm({
  auth,
  open,
  onSave,
  onCancel,
  pending,
}: {
  auth: Extract<AuthTemplate, { type: "token" }>;
  open: boolean;
  onSave: Save;
  onCancel: () => void;
  pending: boolean;
}) {
  const defaults = (): TokenValues => ({
    inputs: auth.inputs.map((input) => ({
      key: input.key,
      label: input.label,
      secret: input.secret,
      placeholder: input.placeholder ?? "",
      help: input.help ?? "",
      pattern: input.pattern ?? "",
    })),
    header: auth.placement.name,
    prefix: auth.placement.prefix,
    verify_url: auth.verify_url ?? "",
    instructions: auth.instructions,
    docs_url: auth.docs_url ?? "",
  });
  const form = useForm<TokenValues>({ resolver: zodResolver(tokenSchema), defaultValues: defaults() });
  const inputs = useFieldArray({ control: form.control, name: "inputs" });
  // biome-ignore lint/correctness/useExhaustiveDependencies: reset to the stored template each time the sheet opens
  useEffect(() => {
    if (open) form.reset(defaults());
  }, [open]);
  const header = form.watch("header");
  const prefix = form.watch("prefix");
  const rows = form.watch("inputs");

  return (
    <Form {...form}>
      <form
        id="edit-auth"
        className="flex-1 space-y-5 overflow-y-auto p-4"
        onSubmit={form.handleSubmit((values) =>
          onSave(
            {
              type: "token",
              inputs: values.inputs.map((input) => ({
                key: input.key,
                label: input.label,
                secret: input.secret,
                placeholder: orNull(input.placeholder),
                help: orNull(input.help),
                pattern: orNull(input.pattern),
              })),
              placement: { location: "header", name: values.header, prefix: values.prefix },
              verify_url: orNull(values.verify_url),
              instructions: values.instructions,
              docs_url: orNull(values.docs_url),
            },
            {
              onSuccess: onCancel,
              onError: (error) => {
                if (!applyFieldErrors(form, error, TOKEN_FIELDS)) toastError(error, "Couldn't save authentication");
              },
            },
          ),
        )}
      >
        <fieldset className="space-y-3">
          <div>
            <legend className="text-sm font-medium">What Connect asks for</legend>
            <p className="text-sm text-muted-foreground">
              Stored encrypted, together, for one installation. Secret values are masked as they're typed.
            </p>
          </div>
          <ul className="space-y-3">
            {inputs.fields.map((input, index) => (
              <li key={input.id} className="space-y-3 rounded-lg border p-3">
                <div className="flex items-center justify-between gap-2">
                  <p className="text-xs font-medium text-muted-foreground">Value {index + 1}</p>
                  <RemoveButton
                    label={`Remove value ${index + 1}`}
                    disabled={inputs.fields.length === 1}
                    onClick={() => inputs.remove(index)}
                  />
                </div>
                <div className="grid grid-cols-2 items-start gap-2">
                  <FormField
                    control={form.control}
                    name={`inputs.${index}.label`}
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel className="text-xs text-muted-foreground">Label</FormLabel>
                        <FormControl>
                          <Input placeholder="API token" {...field} />
                        </FormControl>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                  <FormField
                    control={form.control}
                    name={`inputs.${index}.key`}
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel className="text-xs text-muted-foreground">Key</FormLabel>
                        <FormControl>
                          <Input
                            className="font-mono text-sm"
                            placeholder="token"
                            autoComplete="off"
                            spellCheck={false}
                            {...field}
                          />
                        </FormControl>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                </div>
                <div className="grid grid-cols-2 gap-2">
                  <FormField
                    control={form.control}
                    name={`inputs.${index}.placeholder`}
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel className="text-xs text-muted-foreground">Placeholder</FormLabel>
                        <FormControl>
                          <Input className="font-mono text-sm" placeholder="tk_…" {...field} />
                        </FormControl>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                  <FormField
                    control={form.control}
                    name={`inputs.${index}.help`}
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel className="text-xs text-muted-foreground">Help</FormLabel>
                        <FormControl>
                          <Input placeholder="Where to find it" {...field} />
                        </FormControl>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                </div>
                <FormField
                  control={form.control}
                  name={`inputs.${index}.secret`}
                  render={({ field }) => (
                    <FormItem className="flex items-center gap-2">
                      <FormControl>
                        <Checkbox
                          checked={field.value}
                          onCheckedChange={(checked) => field.onChange(checked === true)}
                        />
                      </FormControl>
                      <FormLabel className="font-normal">Secret</FormLabel>
                    </FormItem>
                  )}
                />
                {rows[index]?.pattern ? (
                  <p className="text-xs text-muted-foreground">
                    Connect checks values against{" "}
                    <code className="rounded bg-muted px-1 py-0.5 text-[11px] break-all">{rows[index]?.pattern}</code>.
                    Set through the API.
                  </p>
                ) : null}
              </li>
            ))}
          </ul>
          <ListMessage form={form} name="inputs" />
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={inputs.fields.length >= 10}
            onClick={() => inputs.append({ key: "", label: "", secret: true, placeholder: "", help: "", pattern: "" })}
          >
            <PlusIcon /> Add value
          </Button>
        </fieldset>

        <fieldset className="space-y-3">
          <legend className="text-sm font-medium">How it's sent</legend>
          <div className="grid grid-cols-[minmax(0,1.2fr)_minmax(0,1fr)] items-start gap-2">
            <FormField
              control={form.control}
              name="header"
              render={({ field }) => (
                <FormItem>
                  <FormLabel className="text-xs text-muted-foreground">Header</FormLabel>
                  <FormControl>
                    <Input className="font-mono text-sm" autoComplete="off" spellCheck={false} {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
            <FormField
              control={form.control}
              name="prefix"
              render={({ field }) => (
                <FormItem>
                  <FormLabel className="text-xs text-muted-foreground">Prefix</FormLabel>
                  <FormControl>
                    <Input className="font-mono text-sm" autoComplete="off" spellCheck={false} {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
          </div>
          <p className="rounded-md bg-muted px-2.5 py-1.5 font-mono text-[12px] text-muted-foreground">
            {header || "Header"}: {prefix}
            {"<token>"}
          </p>
        </fieldset>

        <UrlField
          form={form}
          name="verify_url"
          label="Verify URL"
          placeholder="https://api.provider.example/me"
          description="Optional. A read-only GET Zupflow sends with the new credential before saving it."
        />
        <FormField
          control={form.control}
          name="instructions"
          render={({ field }) => (
            <FormItem>
              <FormLabel>Instructions</FormLabel>
              <FormControl>
                <Textarea rows={3} placeholder="Where to create a token, and what it needs access to" {...field} />
              </FormControl>
              <FormDescription>Shown above the form in Connect.</FormDescription>
              <FormMessage />
            </FormItem>
          )}
        />
        <UrlField form={form} name="docs_url" label="Docs URL" placeholder="https://docs.provider.example/tokens" />
      </form>
      <Footer form="edit-auth" onCancel={onCancel} pending={pending} />
    </Form>
  );
}
