package io.gitlab.hongtao1207.nudge.app

import android.util.TypedValue
import android.widget.TextView
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.LocalTextStyle
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.isSpecified
import androidx.compose.ui.viewinterop.AndroidView
import io.noties.markwon.Markwon
import io.noties.markwon.ext.strikethrough.StrikethroughPlugin
import io.noties.markwon.ext.tables.TablePlugin
import io.noties.markwon.ext.tasklist.TaskListPlugin
import io.noties.markwon.linkify.LinkifyPlugin

// Full-fidelity Markdown for assistant text, backed by Markwon (CommonMark + GFM tables,
// strikethrough, task lists, autolinks). Rendered into a TextView via AndroidView — the
// agent replies in Markdown and tables were the gap the previous hand-rolled subset
// couldn't cover. Colors/size are pulled from the surrounding Compose theme so it matches
// the bubble it sits in.
@Composable
fun MarkdownText(
    text: String,
    modifier: Modifier = Modifier,
    style: TextStyle = LocalTextStyle.current,
) {
    val context = LocalContext.current
    val markwon = remember(context) {
        Markwon.builder(context)
            .usePlugin(TablePlugin.create(context))
            .usePlugin(StrikethroughPlugin.create())
            .usePlugin(TaskListPlugin.create(context))
            .usePlugin(LinkifyPlugin.create())
            .build()
    }
    val contentColor = LocalContentColor.current.toArgb()
    val fontSizeSp: Float? = style.fontSize.takeIf { it.isSpecified }?.value

    AndroidView(
        modifier = modifier,
        factory = { ctx ->
            TextView(ctx).apply {
                setTextColor(contentColor)
                setLinkTextColor(contentColor)
            }
        },
        update = { tv ->
            tv.setTextColor(contentColor)
            tv.setLinkTextColor(contentColor)
            fontSizeSp?.let { tv.setTextSize(TypedValue.COMPLEX_UNIT_SP, it) }
            markwon.setMarkdown(tv, text)
        },
    )
}
