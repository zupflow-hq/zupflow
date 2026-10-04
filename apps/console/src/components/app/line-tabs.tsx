import { TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";

/**
 * Underline tabs. The active bar sits on the list's bottom border (it overlaps
 * the 1px rule exactly) rather than floating above it.
 */
export function LineTabsList({ className, ...props }: React.ComponentProps<typeof TabsList>) {
  return (
    <div className="-mx-4 overflow-x-auto px-4 sm:mx-0 sm:px-0">
      <TabsList
        variant="line"
        className={cn("h-auto w-full min-w-max justify-start gap-5 rounded-none border-b p-0", className)}
        {...props}
      />
    </div>
  );
}

export function LineTabsTrigger({ className, ...props }: React.ComponentProps<typeof TabsTrigger>) {
  return (
    <TabsTrigger
      className={cn(
        "h-10 flex-none rounded-none border-0 px-0.5 text-muted-foreground data-[state=active]:text-foreground dark:data-[state=active]:border-0",
        "group-data-[orientation=horizontal]/tabs:after:bottom-[-1px] after:bg-brand-ink dark:after:bg-brand",
        className,
      )}
      {...props}
    />
  );
}
