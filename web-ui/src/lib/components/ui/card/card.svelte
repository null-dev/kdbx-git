<script lang="ts">
	import { cn } from '$lib/utils';

	let { class: className = '', children } = $props<{
		class?: string;
		children?: import('svelte').Snippet;
	}>();
</script>

<section
	class={cn(
		'rounded-[calc(var(--radius)+0.35rem)] border border-border/70 bg-card/90 text-card-foreground shadow-[0_14px_38px_-24px_rgba(84,54,22,0.32)] backdrop-blur-sm',
		className
	)}
>
	{@render children?.()}
</section>
