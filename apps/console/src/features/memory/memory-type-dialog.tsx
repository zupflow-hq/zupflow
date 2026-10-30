import { type MemoryType, MemoryWrites, type Organization, ZupflowApiError } from "@zupflow/sdk";
import { useEffect } from "react";
import { type FieldErrors, type Resolver, type UseFormReturn, useForm } from "react-hook-form";
import { toast } from "sonner";
import { ChoiceCards } from "@/components/app/choice-cards";
import { QueryError } from "@/components/app/misc";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import {
  useCreateMemoryType,
  useDefaultMemoryType,
  useMemoryKinds,
  useSetDefaultMemoryType,
  useUpdateMemoryType,
} from "@/features/queries";
import { isApiError } from "@/lib/errors";
import { characters, memoryTypeRefusal, WRITES } from "./labels";
import {
  boundsRule,
  defaultLabel,
  formValues,
  lengthRule,
  parsedSetting,
  type SettingSpec,
  type SettingValue,
  settingSpecs,
  settingsToSend,
  settingText,
} from "./settings";

const MAX_NAME = 60;

interface TypeValues {
  name: string;
  /** The kind's key: chosen when creating, fixed afterwards. */
  kind: string;
  writes: MemoryWrites;
  settings: Record<string, SettingValue>;
  /** Creating only, while the organization has no default: make the new type its default. */
  makeDefault: boolean;
}

/**
 * Creates a memory type, or edits one: its name, its kind's settings and
 * whether its agents write with their tools. The kind is chosen once. A type
 * of a kind this server doesn't serve keeps its settings as they are: nothing
 * here can check them.
 */
export function MemoryTypeDialog({
  org,
  type,
  open,
  onOpenChange,
}: {
  org: Organization;
  /** The type to edit; none to create one. */
  type: MemoryType | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const kinds = useMemoryKinds(org.id);
  const current = useDefaultMemoryType(org.id);
  const create = useCreateMemoryType(org.id);
  const update = useUpdateMemoryType(org.id);
  const setDefault = useSetDefaultMemoryType(org.id);
  const mutation = type ? update : create;
  const served = kinds.data ?? [];
  const kindOf = (key: string) => served.find((k) => k.kind === key);
  const specsOf = (key: string) => {
    const kind = kindOf(key);
    return kind ? settingSpecs(kind) : [];
  };
  // Offered only while there's none: choosing among types is the default panel's job.
  const offerDefault = !type && current.data !== undefined && current.data.memory_type === null;

  const resolver: Resolver<TypeValues> = async (values) => {
    const errors: FieldErrors<TypeValues> = {};
    const name = values.name.trim();
    if (!name) errors.name = { type: "required", message: "Name the memory type." };
    else if (characters(name) > MAX_NAME) {
      errors.name = { type: "maxLength", message: `Use ${MAX_NAME} characters or fewer.` };
    }
    if (!type && !kindOf(values.kind)) errors.kind = { type: "required", message: "Choose a kind." };
    const settingErrors: Record<string, { type: string; message: string }> = {};
    for (const spec of specsOf(values.kind)) {
      const parsed = parsedSetting(spec, values.settings[spec.key]);
      if (parsed.error !== undefined) settingErrors[spec.key] = { type: "validate", message: parsed.error };
    }
    if (Object.keys(settingErrors).length > 0) errors.settings = settingErrors;
    return Object.keys(errors).length > 0 ? { values: {}, errors } : { values, errors: {} };
  };
  const form = useForm<TypeValues>({
    resolver,
    defaultValues: { name: "", kind: "", writes: "allow", settings: {}, makeDefault: true },
  });

  // Each opening starts from the type (a snapshot the caller holds while it's open), or from the first
  // kind's defaults, and forgets an earlier refusal. The kinds arriving after it opened start it again.
  const { reset: resetForm } = form;
  const { reset: resetCreate } = create;
  const { reset: resetUpdate } = update;
  const startKind = type ? kindOf(type.kind) : served[0];
  useEffect(() => {
    if (!open) return;
    resetCreate();
    resetUpdate();
    resetForm({
      name: type?.name ?? "",
      kind: type?.kind ?? startKind?.kind ?? "",
      writes: type?.writes ?? "allow",
      settings: startKind ? formValues(settingSpecs(startKind), type?.settings) : {},
      makeDefault: !type,
    });
  }, [open, type, startKind, resetForm, resetCreate, resetUpdate]);

  const kind = form.watch("kind");
  const specs = specsOf(kind);
  // A type whose kind this server doesn't serve: its settings can't be checked, so they aren't sent.
  const unchecked = type !== null && !kindOf(type.kind);
  const refusal = mutation.isError && !landsOnField(mutation.error) ? memoryTypeRefusal(mutation.error) : null;

  const submit = form.handleSubmit((values) => {
    // A setting left empty is left out, and the kind fills in its default.
    const settings = settingsToSend(specsOf(values.kind), values.settings);
    const onError = (error: unknown) => applyTypeErrors(form, error);
    if (type) {
      update.mutate(
        {
          id: type.id,
          name: values.name.trim(),
          writes: values.writes,
          ...(unchecked ? {} : { settings }),
        },
        {
          onSuccess: (saved) => {
            toast.success(`Saved ${saved.name}`);
            onOpenChange(false);
          },
          onError,
        },
      );
      return;
    }
    create.mutate(
      { name: values.name.trim(), kind: values.kind, settings, writes: values.writes },
      {
        onSuccess: (created) => {
          onOpenChange(false);
          if (!(offerDefault && values.makeDefault)) {
            toast.success(`Created ${created.name}`);
            return;
          }
          setDefault.mutate(created.id, {
            onSuccess: () => toast.success(`Created ${created.name} and made it the default`),
            onError: (error) =>
              toast.error(`Created ${created.name}, but couldn't make it the default`, {
                description: memoryTypeRefusal(error),
              }),
          });
        },
        onError,
      },
    );
  });

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90dvh] overflow-y-auto sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{type ? `Edit ${type.name}` : "Create memory type"}</DialogTitle>
          <DialogDescription>
            {type
              ? "Changes apply to the next runs of every agent that uses it, in Sandbox and Production."
              : "A memory kind this server serves, set up for the organization's agents. Sandbox and Production use the same types."}
          </DialogDescription>
        </DialogHeader>
        {kinds.isError ? (
          <QueryError error={kinds.error} onRetry={() => kinds.refetch()} what="the memory kinds" />
        ) : kinds.isPending ? (
          <div className="space-y-3" role="status" aria-label="Loading">
            <Skeleton className="h-9 w-full" />
            <Skeleton className="h-20 w-full" />
            <Skeleton className="h-9 w-2/3" />
          </div>
        ) : !type && served.length === 0 ? (
          <p className="rounded-lg border bg-muted/40 p-3 text-sm">
            This server serves no memory kinds, so no memory type can be created. Whoever runs it chooses the kinds it
            serves.
          </p>
        ) : (
          <Form {...form}>
            <form id="memory-type" noValidate className="min-w-0 space-y-5" onSubmit={submit}>
              <FormField
                control={form.control}
                name="name"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Name</FormLabel>
                    <FormControl>
                      <Input autoComplete="off" placeholder="Notes" {...field} />
                    </FormControl>
                    <FormDescription>What people choose for an agent. Unique in the organization.</FormDescription>
                    <FormMessage />
                  </FormItem>
                )}
              />
              {type ? (
                <div className="space-y-1">
                  <p className="text-sm font-medium">Kind</p>
                  <p className="text-sm">
                    {kindOf(type.kind)?.name ?? type.kind}{" "}
                    <span className="text-muted-foreground">· fixed once a type is created</span>
                  </p>
                </div>
              ) : (
                <FormField
                  control={form.control}
                  name="kind"
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>Kind</FormLabel>
                      <ChoiceCards
                        label="Kind"
                        value={field.value}
                        onValueChange={(next) => {
                          field.onChange(next);
                          const chosen = kindOf(next);
                          form.setValue("settings", chosen ? formValues(settingSpecs(chosen)) : {});
                        }}
                        options={served.map((k) => ({
                          value: k.kind,
                          content: (
                            <>
                              <span className="text-sm font-medium">{k.name}</span>
                              <span className="text-xs text-muted-foreground">{k.description}</span>
                            </>
                          ),
                        }))}
                      />
                      <FormMessage />
                    </FormItem>
                  )}
                />
              )}
              {unchecked && type ? (
                <div className="space-y-2">
                  <p className="text-sm font-medium">Settings</p>
                  <p className="text-sm text-muted-foreground">
                    This server doesn't serve {type.kind}, so it can't check settings. They stay as they are.
                  </p>
                  {Object.keys(type.settings).length > 0 ? (
                    <dl className="divide-y rounded-lg border text-sm">
                      {Object.entries(type.settings).map(([key, value]) => (
                        <div key={key} className="flex flex-wrap justify-between gap-x-4 gap-y-1 px-3 py-2">
                          <dt className="font-mono text-xs text-muted-foreground">{key}</dt>
                          <dd className="min-w-0 break-all">{settingText(value)}</dd>
                        </div>
                      ))}
                    </dl>
                  ) : null}
                </div>
              ) : specs.length > 0 ? (
                <fieldset className="min-w-0 space-y-4">
                  <legend className="mb-3 text-sm font-medium">Settings</legend>
                  {specs.map((spec) => (
                    <SettingField key={`${kind}.${spec.key}`} form={form} spec={spec} />
                  ))}
                </fieldset>
              ) : null}
              <FormField
                control={form.control}
                name="writes"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Agent writes</FormLabel>
                    <FormDescription>Whether agents save, change and forget memories with their tools.</FormDescription>
                    <ChoiceCards<MemoryWrites>
                      label="Agent writes"
                      value={field.value}
                      onValueChange={field.onChange}
                      options={MemoryWrites.options.map((writes) => ({
                        value: writes,
                        content: (
                          <>
                            <span className="text-sm font-medium">{WRITES[writes].label}</span>
                            <span className="text-xs text-muted-foreground">{WRITES[writes].description}</span>
                          </>
                        ),
                      }))}
                    />
                    <FormMessage />
                  </FormItem>
                )}
              />
              {offerDefault ? (
                <FormField
                  control={form.control}
                  name="makeDefault"
                  render={({ field }) => (
                    <FormItem className="flex items-start gap-3 rounded-lg border bg-muted/40 p-3">
                      <FormControl>
                        <Checkbox
                          checked={field.value}
                          onCheckedChange={(on) => field.onChange(on === true)}
                          className="mt-0.5"
                        />
                      </FormControl>
                      <div className="space-y-1">
                        <FormLabel className="font-medium">Make it the default</FormLabel>
                        <FormDescription>
                          The organization has no default memory type, so agents on the default have no memory until one
                          is chosen.
                        </FormDescription>
                      </div>
                    </FormItem>
                  )}
                />
              ) : null}
              {refusal ? (
                <p role="alert" className="rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-sm">
                  {refusal}
                </p>
              ) : null}
            </form>
          </Form>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            type="submit"
            form="memory-type"
            disabled={mutation.isPending || kinds.isPending || kinds.isError || (!type && served.length === 0)}
          >
            {mutation.isPending ? <Spinner /> : null}
            {type ? "Save" : "Create memory type"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** Whether a refusal names fields the form shows. */
function landsOnField(error: unknown): boolean {
  if (!(error instanceof ZupflowApiError) || !error.fields) return false;
  return Object.keys(error.fields).some(
    (field) => field === "name" || field === "kind" || field === "writes" || field.startsWith("settings."),
  );
}

/** Puts a refusal's fields on the form: the name, the kind, the writes, or a setting by its key. */
function applyTypeErrors(form: UseFormReturn<TypeValues>, error: unknown) {
  if (!(error instanceof ZupflowApiError) || !error.fields) return;
  for (const [field, message] of Object.entries(error.fields)) {
    if (field === "name") {
      form.setError("name", {
        type: "server",
        message: isApiError(error, "memory_type_name_taken")
          ? "Another memory type has this name. Choose another."
          : message,
      });
    } else if (field === "kind" || field === "writes") {
      form.setError(field, { type: "server", message });
    } else if (field.startsWith("settings.")) {
      form.setError(`settings.${field.slice("settings.".length)}`, { type: "server", message });
    }
  }
}

/** One setting, as a field of its type, with its kind's description, bounds and default. */
function SettingField({ form, spec }: { form: UseFormReturn<TypeValues>; spec: SettingSpec }) {
  const fallback = defaultLabel(spec);
  const rule =
    spec.type === "integer" || spec.type === "number"
      ? boundsRule(spec)
      : spec.type === "string"
        ? lengthRule(spec)
        : null;
  // An optional setting with no default the schema names: left empty, the kind decides.
  const optional =
    !spec.required && spec.type !== "boolean" && fallback === null ? "Leave it empty for the kind's default." : null;
  const help = [spec.description, rule, fallback === null ? null : `Default: ${fallback}.`, optional]
    .filter(Boolean)
    .join(" ");
  return (
    <FormField
      control={form.control}
      name={`settings.${spec.key}`}
      render={({ field }) => (
        <FormItem>
          {spec.type === "boolean" ? (
            <div className="flex items-center justify-between gap-4">
              <FormLabel>{spec.label}</FormLabel>
              <FormControl>
                <Switch checked={field.value === true} onCheckedChange={field.onChange} />
              </FormControl>
            </div>
          ) : (
            <>
              <FormLabel>{spec.label}</FormLabel>
              {spec.type === "choice" ? (
                <Select value={typeof field.value === "string" ? field.value : ""} onValueChange={field.onChange}>
                  {/* The trigger is the control: the label and the description attach to it. */}
                  <FormControl>
                    <SelectTrigger className="w-full bg-card" onBlur={field.onBlur} ref={field.ref}>
                      <SelectValue placeholder="Choose" />
                    </SelectTrigger>
                  </FormControl>
                  <SelectContent>
                    {spec.options.map((option) => (
                      <SelectItem key={String(option)} value={String(option)}>
                        {typeof option === "number" ? option.toLocaleString("en") : option}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              ) : (
                <FormControl>
                  {spec.type === "json" ? (
                    <Textarea
                      rows={3}
                      className="font-mono text-[13px]"
                      value={typeof field.value === "string" ? field.value : ""}
                      onChange={field.onChange}
                      onBlur={field.onBlur}
                      name={field.name}
                      ref={field.ref}
                    />
                  ) : (
                    <Input
                      type={spec.type === "string" ? "text" : "number"}
                      inputMode={spec.type === "integer" ? "numeric" : spec.type === "number" ? "decimal" : undefined}
                      autoComplete="off"
                      className="sm:max-w-48"
                      value={typeof field.value === "string" ? field.value : ""}
                      onChange={field.onChange}
                      onBlur={field.onBlur}
                      name={field.name}
                      ref={field.ref}
                    />
                  )}
                </FormControl>
              )}
            </>
          )}
          {help ? <FormDescription>{help}</FormDescription> : null}
          <FormMessage />
        </FormItem>
      )}
    />
  );
}
