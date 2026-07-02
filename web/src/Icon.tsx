import { ICON_PREFIX, ICON_SPRITE, type IconName } from "./icons";

export function IconSprite() {
  return <div class="icon-sprite" aria-hidden="true" innerHTML={ICON_SPRITE} />;
}

export default function Icon(props: { name: IconName; class?: string }) {
  return (
    <svg class={props.class ? `icon ${props.class}` : "icon"} aria-hidden="true">
      <use href={`#${ICON_PREFIX}${props.name}`} />
    </svg>
  );
}
