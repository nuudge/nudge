package io.gitlab.hongtao1207.nudge.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier

class MainActivity : ComponentActivity() {
    private val viewModel: ChatViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        // Draw edge-to-edge so the system applies the system-bar and IME insets exactly
        // once; the screen's systemBarsPadding()/imePadding() then consume them. Without
        // this the platform also resizes for the keyboard, double-counting the inset and
        // shoving the transcript up off the input box.
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    ChatScreen(viewModel)
                }
            }
        }
    }

    // Fires on launch and every return to the foreground — re-attach from the saved
    // pairing if we're not already connected, so switching apps or relaunching doesn't
    // require a re-scan.
    override fun onStart() {
        super.onStart()
        viewModel.resume()
    }
}
