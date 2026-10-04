import { ZupflowApiError, ZupflowNetworkError } from "@zupflow/sdk";
import type { FieldValues, Path, UseFormReturn } from "react-hook-form";
import { toast } from "sonner";

export function errorMessage(error: unknown): string {
  if (error instanceof ZupflowApiError) return error.message;
  if (error instanceof ZupflowNetworkError) return "Can't reach Zupflow. Check your connection and retry.";
  return "Something went wrong. Try again.";
}

export function isApiError(error: unknown, ...codes: string[]): error is ZupflowApiError {
  return error instanceof ZupflowApiError && (codes.length === 0 || codes.includes(error.code));
}

export function isStatus(error: unknown, status: number): boolean {
  return error instanceof ZupflowApiError && error.status === status;
}

/**
 * Put server field errors on the form. Returns true when at least one field
 * matched, so the caller can skip a redundant toast.
 */
export function applyFieldErrors<T extends FieldValues>(
  form: UseFormReturn<T>,
  error: unknown,
  map: Partial<Record<string, Path<T>>> = {},
): boolean {
  if (!(error instanceof ZupflowApiError) || !error.fields) return false;
  let matched = false;
  for (const [field, message] of Object.entries(error.fields)) {
    const target = map[field] ?? (field in form.getValues() ? (field as Path<T>) : undefined);
    if (target) {
      form.setError(target, { type: "server", message });
      matched = true;
    }
  }
  return matched;
}

export function toastError(error: unknown, title?: string) {
  toast.error(title ?? errorMessage(error), title ? { description: errorMessage(error) } : undefined);
}
