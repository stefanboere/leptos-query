use super::*;
use leptos::prelude::*;
use leptos::either::EitherOf5;

#[component]
pub fn Button(
    children: ChildrenFn,
    color: ColorOption,
) -> impl IntoView {
    match color {
        ColorOption::Blue => EitherOf5::A(view! {
            <button
                type="button"
                class="lq-text-white lq-bg-blue-700 lq-hover:bg-blue-800 lq-focus:outline-none lq-focus:ring-4 lq-focus:ring-blue-300 lq-font-medium lq-rounded-full lq-text-xs lq-px-2 lq-py-1 lq-text-center lq-dark:bg-blue-600 lq-dark:hover:bg-blue-700 lq-dark:focus:ring-blue-800"
            >
                {children()}
            </button>
        }),
        ColorOption::Green => EitherOf5::B(view! {
            <button
                type="button"
                class="lq-text-white lq-bg-green-700 lq-hover:bg-green-800 lq-focus:outline-none lq-focus:ring-4 lq-focus:ring-green-300 lq-font-medium lq-rounded-full lq-text-xs lq-px-2 lq-py-1 lq-text-center lq-dark:bg-green-600 lq-dark:hover:bg-green-700 lq-dark:focus:ring-green-800"
            >
                {children()}
            </button>
        }),
        ColorOption::Red => EitherOf5::C(view! {
            <button
                type="button"
                class="lq-text-white lq-bg-red-700 lq-hover:bg-red-800 lq-focus:outline-none lq-focus:ring-4 lq-focus:ring-red-300 lq-font-medium lq-rounded-full lq-text-xs lq-px-2 lq-py-1 lq-text-center lq-dark:bg-red-600 lq-dark:hover:bg-red-700 lq-dark:focus:ring-red-900"
            >
                {children()}
            </button>
        }),
        ColorOption::Yellow => EitherOf5::D(view! {
            <button
                type="button"
                class="lq-text-white lq-bg-yellow-400 lq-hover:bg-yellow-500 lq-focus:outline-none lq-focus:ring-4 lq-focus:ring-yellow-300 lq-font-medium lq-rounded-full lq-text-xs lq-px-2 lq-py-1 lq-text-center lq-dark:focus:ring-yellow-900"
            >
                {children()}
            </button>
        }),
        ColorOption::Gray => EitherOf5::E(view! {
            <button
                type="button"
                class="lq-text-gray-900 lq-bg-white lq-border lq-border-gray-300 lq-focus:outline-none lq-hover:bg-gray-100 lq-focus:ring-4 lq-focus:ring-gray-200 lq-font-medium lq-rounded-full lq-text-xs lq-px-2 lq-py-1 lq-dark:bg-gray-800 lq-dark:text-white lq-dark:border-gray-600 lq-dark:hover:bg-gray-700 lq-dark:hover:border-gray-600 lq-dark:focus:ring-gray-700"
            >
                {children()}
            </button>
        }),
    }
}
