import * as React from "react"
import { cva, type VariantProps } from "class-variance-authority"

import { cn } from "@/lib/utils"

const badgeVariants = cva(
  "inline-flex items-center rounded-full border px-2 py-0.5 text-[11px] font-medium transition-colors focus:outline-none focus:ring-2 focus:ring-white/20",
  {
    variants: {
      variant: {
        default:
          "border-white/10 bg-white/[.07] text-zinc-300",
        secondary:
          "border-white/10 bg-white/10 text-zinc-100",
        destructive:
          "border-red-400/[.25] bg-red-500/[.18] text-red-100",
        outline: "border-white/[.14] text-zinc-200",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  }
)

export interface BadgeProps
  extends React.HTMLAttributes<HTMLDivElement>,
    VariantProps<typeof badgeVariants> {}

function Badge({ className, variant, ...props }: BadgeProps) {
  return (
    <div className={cn(badgeVariants({ variant }), className)} {...props} />
  )
}

export { Badge, badgeVariants }
