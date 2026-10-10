import { zodResolver } from "@hookform/resolvers/zod";
import type { AppDefinition, AuthType, CreateAppInput, Environment } from "@zupflow/sdk";
import { KeyRoundIcon, LogInIcon, ShieldCheckIcon } from "lucide-react";
import { useEffect } from "react";
import { useForm } from "react-hook-form";
import { z } from "zod";
import { ChoiceCards } from "@/components/app/choice-cards";
import { EnvBadge } from "@/components/app/environment";
import { Button } from "@/components/ui/button";
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
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { applyFieldErrors, toastError } from "@/lib/errors";
import { slugify } from "@/lib/format";

const schema = z.object({
  kind: z.literal("mcp"),
  name: z.string().trim().min(1, "Name the app.").max(60),
  slug: z.string().regex(/^[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?$/, "Use lowercase letters, numbers and hyphens."),
  description: z.string().trim().max(280),
  auth_type: z.enum(["mcp_oauth", "oauth2", "token"], { message: "Choose how accounts authenticate." }),
});
type Values = z.infer<typeof schema>;

const AUTH: Record<AuthType, { title: string; body: string; icon: typeof KeyRoundIcon }> = {
  mcp_oauth: {
    title: "Sign in through the MCP server",
    body: "Users sign in on the server's own screen. Zupflow finds its sign-in and registers a client; you set only the server URL.",
    icon: LogInIcon,
  },
  oauth2: {
    title: "OAuth",
    body: "Users approve access on the provider's consent screen. You register a client per environment.",
    icon: ShieldCheckIcon,
  },
  token: {
    title: "API token",
    body: "Users paste a token in Connect. Zupflow stores it encrypted for that installation only.",
    icon: KeyRoundIcon,
  },
};

/**
 * Creates an app definition. Organization apps join the shared catalog;
 * workspace apps stay private to one workspace and inherit its environment.
 */
export function CreateAppDialog({
  open,
  onOpenChange,
  scope,
  onCreate,
  pending,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  scope: { type: "organization" } | { type: "workspace"; name: string; environment: Environment };
  onCreate: (
    input: CreateAppInput,
    callbacks: { onSuccess: (app: AppDefinition) => void; onError: (error: unknown) => void },
  ) => void;
  pending: boolean;
}) {
  const form = useForm<Values>({
    resolver: zodResolver(schema),
    defaultValues: { kind: "mcp", name: "", slug: "", description: "", auth_type: undefined },
  });
  const name = form.watch("name");
  const slugTouched = form.formState.dirtyFields.slug;

  useEffect(() => {
    if (!slugTouched) form.setValue("slug", slugify(name));
  }, [name, slugTouched, form]);

  useEffect(() => {
    if (!open) form.reset();
  }, [open, form]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{scope.type === "organization" ? "Add app to catalog" : "Create workspace app"}</DialogTitle>
          <DialogDescription>
            {scope.type === "organization" ? (
              "One definition for the whole organization. Configure Sandbox and Production separately afterwards."
            ) : (
              <>
                Private to <span className="font-medium text-foreground">{scope.name}</span>. It inherits the
                workspace's environment{" "}
                <EnvBadge environment={scope.environment} size="sm" className="mx-0.5 align-middle" /> and no other
                workspace can see it.
              </>
            )}
          </DialogDescription>
        </DialogHeader>
        <Form {...form}>
          <form
            id="create-app"
            className="space-y-5"
            onSubmit={form.handleSubmit((values) =>
              onCreate(values, {
                onSuccess: () => onOpenChange(false),
                onError: (error) => {
                  if (!applyFieldErrors(form, error)) toastError(error, "Couldn't create the app");
                },
              }),
            )}
          >
            <div className="grid gap-4 sm:grid-cols-[1fr_8.5rem]">
              <FormField
                control={form.control}
                name="name"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Name</FormLabel>
                    <FormControl>
                      <Input placeholder="Notion" autoComplete="off" {...field} />
                    </FormControl>
                    <FormMessage />
                  </FormItem>
                )}
              />
              <FormField
                control={form.control}
                name="kind"
                render={({ field }) => (
                  <FormItem>
                    <FormLabel>Kind</FormLabel>
                    <Select value={field.value} onValueChange={field.onChange}>
                      <FormControl>
                        <SelectTrigger className="w-full font-mono text-sm">
                          <SelectValue />
                        </SelectTrigger>
                      </FormControl>
                      <SelectContent>
                        <SelectItem value="mcp" className="font-mono">
                          mcp
                        </SelectItem>
                      </SelectContent>
                    </Select>
                    <FormMessage />
                  </FormItem>
                )}
              />
            </div>
            <FormField
              control={form.control}
              name="slug"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Slug</FormLabel>
                  <FormControl>
                    <Input className="font-mono text-sm" autoComplete="off" {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
            <FormField
              control={form.control}
              name="description"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>
                    Description <span className="font-normal text-muted-foreground">(optional)</span>
                  </FormLabel>
                  <FormControl>
                    <Textarea rows={2} placeholder="What agents can do with it" {...field} />
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />
            <FormField
              control={form.control}
              name="auth_type"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>Authentication</FormLabel>
                  <ChoiceCards
                    label="Authentication"
                    value={field.value}
                    onValueChange={field.onChange}
                    options={(["mcp_oauth", "oauth2", "token"] as const).map((type) => {
                      const a = AUTH[type];
                      return {
                        value: type,
                        content: (
                          <>
                            <span className="flex items-center gap-2 text-sm font-medium">
                              <a.icon className="size-4 text-muted-foreground" />
                              {a.title}
                            </span>
                            <span className="text-xs text-muted-foreground">{a.body}</span>
                          </>
                        ),
                      };
                    })}
                  />
                  <FormDescription>Independent of the kind. You can't change it later.</FormDescription>
                  <FormMessage />
                </FormItem>
              )}
            />
          </form>
        </Form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="create-app" disabled={pending}>
            {pending ? <Spinner /> : null}
            {scope.type === "organization" ? "Add app" : "Create app"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
