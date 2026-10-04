import type { UsageDay } from "@zupflow/sdk";
import { format } from "date-fns";
import { Bar, BarChart, CartesianGrid, XAxis, YAxis } from "recharts";
import {
  type ChartConfig,
  ChartContainer,
  ChartLegend,
  ChartLegendContent,
  ChartTooltip,
  ChartTooltipContent,
} from "@/components/ui/chart";

const config = {
  succeeded: { label: "Other runs", color: "var(--chart-1)" },
  failed: { label: "Failed", color: "var(--destructive)" },
} satisfies ChartConfig;

/**
 * Runs per day, with failures stacked on top in the status color. Colors were
 * validated for both themes; a hidden table carries the same numbers.
 */
export function RunsChart({ days, label }: { days: UsageDay[]; label: string }) {
  const data = days.map((d) => ({
    date: d.date,
    succeeded: d.runs - d.failed_runs,
    failed: d.failed_runs,
  }));
  return (
    <figure>
      <ChartContainer config={config} className="aspect-auto h-44 w-full">
        <BarChart data={data} margin={{ top: 4, right: 0, left: -24, bottom: 0 }} barCategoryGap={3}>
          <CartesianGrid vertical={false} strokeDasharray="0" />
          <XAxis
            dataKey="date"
            tickLine={false}
            axisLine={false}
            tickMargin={8}
            minTickGap={24}
            tickFormatter={(v: string) => format(new Date(`${v}T00:00:00Z`), "MMM d")}
          />
          <YAxis tickLine={false} axisLine={false} allowDecimals={false} width={44} />
          <ChartTooltip
            cursor={{ fillOpacity: 0.6 }}
            content={
              <ChartTooltipContent
                labelFormatter={(_, payload) => {
                  const date = payload?.[0]?.payload?.date as string | undefined;
                  return date ? format(new Date(`${date}T00:00:00Z`), "EEEE, MMM d") : "";
                }}
              />
            }
          />
          <ChartLegend content={<ChartLegendContent />} />
          <Bar
            dataKey="succeeded"
            stackId="runs"
            isAnimationActive={false}
            fill="var(--color-succeeded)"
            radius={[4, 4, 0, 0]}
            stroke="var(--card)"
            strokeWidth={1}
          />
          <Bar
            dataKey="failed"
            stackId="runs"
            isAnimationActive={false}
            fill="var(--color-failed)"
            radius={[4, 4, 0, 0]}
            stroke="var(--card)"
            strokeWidth={1}
          />
        </BarChart>
      </ChartContainer>
      <table className="sr-only">
        <caption>{label}</caption>
        <thead>
          <tr>
            <th>Date</th>
            <th>Runs</th>
            <th>Failed</th>
          </tr>
        </thead>
        <tbody>
          {days.map((d) => (
            <tr key={d.date}>
              <td>{d.date}</td>
              <td>{d.runs}</td>
              <td>{d.failed_runs}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </figure>
  );
}
