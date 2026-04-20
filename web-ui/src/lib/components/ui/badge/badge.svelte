<script lang="ts">
	import { cva, type VariantProps } from 'class-variance-authority';

	import { cn } from '$lib/utils';

	const badgeVariants = cva(
		'inline-flex items-center rounded-full border px-2.5 py-1 text-xs font-medium transition-colors',
		{
			variants: {
				variant: {
					default: 'border-transparent bg-primary/12 text-primary',
					secondary: 'border-transparent bg-secondary text-secondary-foreground',
					outline: 'border-border bg-background text-foreground'
				}
			},
			defaultVariants: {
				variant: 'default'
			}
		}
	);

	let {
		class: className = '',
		variant = 'default',
		children
	}: { class?: string; variant?: VariantProps<typeof badgeVariants>['variant']; children?: import('svelte').Snippet } = $props();
</script>

<span class={cn(badgeVariants({ variant }), className)}>
	{@render children?.()}
</span>
